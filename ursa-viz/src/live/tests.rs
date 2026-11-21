use super::*;
use crate::highlight::flag;
use crate::model::{
    BatchValues, Column, EdgeBatch, Encodings, Highlights, KeyType, NodeBatch, NodeLabel, Path,
    Positions, Provenance,
};

/// A result of `n` nodes keyed `0..n`, with these edges and no positions.
fn result(n: u32, edges: &[(u32, u32)]) -> GraphResult {
    let (src, dst) = edges.iter().copied().unzip();
    let base = GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("node", KeyType::Int)),
        Op::AddEdgeLabel("edge".into()),
        Op::AddNodes(NodeBatch::of_label(0, 0..i64::from(n))),
        Op::AddEdges(EdgeBatch::of_label(0, src, dst)),
    ]);
    GraphResult::build(None, Provenance::default(), &base).unwrap()
}

/// A square with a diagonal and a tail: 0-1-2-3-0, 0-2, 3-4.
fn kite() -> GraphResult {
    result(5, &[(0, 1), (1, 2), (2, 3), (3, 0), (0, 2), (3, 4)])
}

/// `count` new nodes after the `n` there are, and these edges.
fn growth(n: u32, count: u32, edges: &[(u32, u32)]) -> GraphDelta {
    let (src, dst) = edges.iter().copied().unzip();
    GraphDelta::new(vec![
        Op::AddNodes(NodeBatch::of_label(
            0,
            i64::from(n)..i64::from(n) + i64::from(count),
        )),
        Op::AddEdges(EdgeBatch::of_label(0, src, dst)),
    ])
}

/// A scene and a stand-in for the GPU's buffers, written only as
/// [`Changes`] say: what `View::apply` does, without a device.
struct Scene {
    live: LiveResult,
    drawn: Drawn,
    theme: Theme,
    gpu: Gpu,
    /// Bytes written by the last delta.
    uploaded: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Gpu {
    x: Vec<u32>,
    y: Vec<u32>,
    radius: Vec<u32>,
    color: Vec<u32>,
    flags: Vec<u8>,
    lines: Vec<u32>,
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|f| f.to_bits()).collect()
}

impl Scene {
    fn new(result: GraphResult) -> Self {
        let theme = Theme::light();
        let (live, mut drawn) = LiveResult::new(result, &theme);
        drawn.highlight.take_dirty();
        let gpu = Gpu {
            x: bits(&drawn.x),
            y: bits(&drawn.y),
            radius: bits(&drawn.style.radius),
            color: drawn.style.color.clone(),
            flags: drawn.highlight.flags().to_vec(),
            lines: live.line_indices(0..drawn.graph.n_edges()),
        };
        Self {
            live,
            drawn,
            theme,
            gpu,
            uploaded: 0,
        }
    }

    /// Apply `delta`, write what it changed, and check that what the GPU
    /// holds is what a view made of the result now would hold.
    fn apply(&mut self, delta: &GraphDelta) -> Result<Changes, ModelError> {
        let changes = self.live.apply(delta, &mut self.drawn.parts(&self.theme))?;
        let (n, m) = (changes.nodes.1, changes.edges.1);
        let (d, gpu) = (&mut self.drawn, &mut self.gpu);
        let mut bytes = 0;
        for column in [&mut gpu.x, &mut gpu.y, &mut gpu.radius, &mut gpu.color] {
            // A slot that is not written keeps what it held: mark the new.
            column.truncate(changes.new_nodes_from.min(n));
            column.resize(n, 0xdead_beef);
        }
        gpu.lines.truncate(2 * changes.new_edges_from.min(m));
        gpu.lines.resize(2 * m, 0xdead_beef);
        gpu.flags.resize(n, 0xee);
        let mut styles = vec![changes.new_nodes_from..n];
        styles.extend(changes.restyled.iter().cloned());
        for run in styles {
            gpu.radius[run.clone()].copy_from_slice(&bits(&d.style.radius[run.clone()]));
            gpu.color[run.clone()].copy_from_slice(&d.style.color[run.clone()]);
            bytes += 8 * run.len() as u64;
        }
        let positions = if changes.positions {
            0..n
        } else {
            changes.new_nodes_from..n
        };
        gpu.x[positions.clone()].copy_from_slice(&bits(&d.x[positions.clone()]));
        gpu.y[positions.clone()].copy_from_slice(&bits(&d.y[positions.clone()]));
        bytes += 8 * positions.len() as u64;
        let mut edges = vec![changes.new_edges_from..m];
        edges.extend(changes.edge_runs.iter().cloned());
        for run in edges {
            gpu.lines[2 * run.start..2 * run.end]
                .copy_from_slice(&self.live.line_indices(run.clone()));
            bytes += 8 * run.len() as u64;
        }
        assert_eq!(
            bytes,
            changes.upload_bytes(),
            "upload_bytes counts the writes"
        );
        for run in d.highlight.take_dirty() {
            gpu.flags[run.clone()].copy_from_slice(&d.highlight.flags()[run.clone()]);
            bytes += 4 * run.len() as u64;
        }
        self.uploaded = bytes;
        self.check();
        Ok(changes)
    }

    /// What is drawn is what a view made of the result as it is now draws,
    /// and the GPU holds it.
    fn check(&self) {
        let (fresh_live, fresh) = LiveResult::new(self.live.result().clone(), &self.theme);
        let d = &self.drawn;
        let n = self.live.result().n_nodes();
        assert_eq!(d.graph.n_nodes(), n);
        assert_eq!(d.graph.src(), self.live.result().src());
        assert_eq!(d.graph.dst(), self.live.result().dst());
        assert_eq!(d.x.len(), n);
        assert_eq!(d.y.len(), n);
        assert!(
            d.x.iter().chain(&d.y).all(|v| v.is_finite()),
            "every node has a place"
        );
        assert_eq!(bits(&d.style.radius), bits(&fresh.style.radius), "radii");
        assert_eq!(d.style.color, fresh.style.color, "colours");
        assert_eq!(d.categories, fresh.categories, "categories");
        assert_eq!(d.highlight.flags(), fresh.highlight.flags(), "flags");
        assert_eq!(
            d.highlight.edges(),
            fresh.highlight.edges(),
            "highlighted edges"
        );
        assert_eq!(self.live.degrees.degree, d.graph.degrees(), "degrees");
        assert_eq!(
            self.live.info(&self.theme),
            fresh_live.info(&self.theme),
            "info"
        );
        let want = Gpu {
            x: bits(&d.x),
            y: bits(&d.y),
            radius: bits(&d.style.radius),
            color: d.style.color.clone(),
            flags: d.highlight.flags().to_vec(),
            lines: self.live.line_indices(0..d.graph.n_edges()),
        };
        assert_eq!(self.gpu, want, "the GPU holds what is drawn");
    }

    fn result(&self) -> &GraphResult {
        self.live.result()
    }
}

#[test]
fn a_view_of_a_result_is_sized_by_degree_and_hides_what_is_removed() {
    let mut r = kite();
    r.apply(&GraphDelta::new(vec![
        Op::RemoveNodes(vec![4]),
        Op::RemoveEdges(vec![4]),
    ]))
    .unwrap();
    let scene = Scene::new(r);
    let d = &scene.drawn;
    // Degrees 3, 2, 3, 3, 1: removed nodes and edges keep their slots, and
    // count as the graph's.
    let radius: Vec<f32> = [3, 2, 3, 3, 1].map(NodeStyle::degree_radius).to_vec();
    assert_eq!(d.style.radius, radius);
    assert_eq!(d.style.color, vec![scene.theme.node; 5]);
    assert_eq!(d.categories, None);
    assert_eq!(d.highlight.flags(), [0, 0, 0, 0, flag::HIDDEN]);
    // The removed edge 0-2 is drawn from 0 to 0: nothing.
    assert_eq!(scene.live.line_indices(3..6), [3, 0, 0, 0, 3, 4]);
    let info = scene.live.info(&scene.theme);
    assert_eq!((info.nodes, info.edges), (4, 4));
    assert_eq!((info.hidden_nodes, info.hidden_edges), (1, 2));
    assert_eq!((info.node_slots, info.edge_slots), (5, 6));
}

#[test]
fn new_nodes_and_edges_take_the_slots_after_the_last() {
    let mut scene = Scene::new(kite());
    let before = scene.gpu.clone();
    // Two new nodes off node 4, and an edge between two old ones.
    let delta = growth(5, 2, &[(4, 5), (4, 6), (1, 3)]);
    let changes = scene.apply(&delta).unwrap();
    assert_eq!(changes.nodes, (5, 7));
    assert_eq!(changes.edges, (6, 9));
    assert_eq!((changes.new_nodes_from, changes.new_edges_from), (5, 6));
    // Nodes 1, 3 and 4 gained a neighbour: one run of older nodes.
    assert_eq!(changes.restyled, vec![1..5]);
    assert!(changes.edge_runs.is_empty());
    assert!(!changes.positions && !changes.highlights);
    // 16 bytes a new node, 8 a restyled one, 8 an edge; 4 a new node's flags.
    assert_eq!(changes.upload_bytes(), 2 * 16 + 4 * 8 + 3 * 8);
    assert_eq!(scene.uploaded, changes.upload_bytes() + 2 * 4);
    // What was there is where it was.
    assert_eq!(scene.gpu.x[..5], before.x[..]);
    assert_eq!(scene.gpu.lines[..12], before.lines[..]);
    assert_eq!(scene.drawn.graph.n_edges(), 9);

    // Its inverse is a shorter range: only the three older nodes are written.
    let back = scene.apply(&delta.invert()).unwrap();
    assert_eq!((back.new_nodes_from, back.new_edges_from), (5, 6));
    assert_eq!(back.upload_bytes(), 4 * 8);
    assert_eq!(scene.gpu, before);
}

#[test]
fn new_nodes_start_where_the_delta_puts_them_or_beside_their_parents() {
    // With positions in the result, a batch brings its own.
    let mut r = kite();
    let at = Positions {
        x: vec![0.0, 10.0, 10.0, 0.0, -10.0],
        y: vec![0.0, 0.0, 10.0, 10.0, 10.0],
    };
    r.apply(&GraphDelta::new(vec![r.set_positions_op(Some(at))]))
        .unwrap();
    let mut scene = Scene::new(r);
    assert_eq!(scene.drawn.x, [0.0, 10.0, 10.0, 0.0, -10.0]);
    let mut batch = NodeBatch::of_label(0, [5i64]);
    batch.positions = Some(Positions {
        x: vec![-20.0],
        y: vec![5.0],
    });
    scene
        .apply(&GraphDelta::new(vec![Op::AddNodes(batch)]))
        .unwrap();
    assert_eq!((scene.drawn.x[5], scene.drawn.y[5]), (-20.0, 5.0));

    // Without, a new node starts beside the node it hangs off; a chain of
    // them one after another; one with no edge around the middle.
    let mut scene = Scene::new(kite());
    let (x, y) = (scene.drawn.x.clone(), scene.drawn.y.clone());
    let (x0, y0, x1, y1) = bounds(&x, &y).unwrap();
    let reach = (x1 - x0).hypot(y1 - y0);
    scene
        .apply(&growth(5, 5, &[(5, 4), (4, 6), (7, 4), (7, 8)]))
        .unwrap();
    let d = &scene.drawn;
    assert_eq!(
        (&d.x[..5], &d.y[..5]),
        (&x[..], &y[..]),
        "nothing drawn moves"
    );
    let far = |a: usize, b: usize| (d.x[a] - d.x[b]).hypot(d.y[a] - d.y[b]);
    for (child, parent) in [(5, 4), (6, 4), (7, 4), (8, 7)] {
        let gap = far(child, parent);
        assert!(gap > 0.0 && gap < reach, "{child} is {gap} from {parent}");
    }
    // Three children of one node are in three places.
    assert!(far(5, 6) > 0.0 && far(6, 7) > 0.0 && far(5, 7) > 0.0);
    let middle = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let off = (d.x[9] - middle.0).hypot(d.y[9] - middle.1);
    assert!(
        off > 0.0 && off < reach,
        "the lone node is {off} from the middle"
    );

    // The same delta on the same scene places them on the same bits.
    let mut again = Scene::new(kite());
    again
        .apply(&growth(5, 5, &[(5, 4), (4, 6), (7, 4), (7, 8)]))
        .unwrap();
    assert_eq!(bits(&again.drawn.x), bits(&scene.drawn.x));
    assert_eq!(bits(&again.drawn.y), bits(&scene.drawn.y));
}

#[test]
fn removed_nodes_are_hidden_by_a_flag_and_removed_edges_in_their_slot() {
    let mut scene = Scene::new(kite());
    let before = scene.gpu.clone();
    let hide = GraphDelta::new(vec![Op::RemoveNodes(vec![3])]);
    let changes = scene.apply(&hide).unwrap();
    // One flags byte on the CPU, four on the GPU: no edge, style or position.
    assert_eq!(changes.upload_bytes(), 0);
    assert_eq!(scene.uploaded, 4);
    assert!(changes.highlights);
    assert_eq!(scene.drawn.highlight.flags()[3], flag::HIDDEN);
    assert!(scene.drawn.highlight.any_hidden());
    assert_eq!(scene.gpu.lines, before.lines, "its edges keep their slots");
    let info = scene.live.info(&scene.theme);
    assert_eq!((info.nodes, info.edges, info.hidden_edges), (4, 3, 3));

    let cut = GraphDelta::new(vec![Op::RemoveEdges(vec![0, 1, 5])]);
    let changes = scene.apply(&cut).unwrap();
    // Slots a few apart are one write.
    assert_eq!(changes.edge_runs, vec![0..6]);
    assert_eq!(changes.upload_bytes(), 6 * 8);
    assert_eq!(scene.gpu.lines[..4], [0, 0, 1, 1]);
    assert_eq!(scene.gpu.lines[10..], [3, 3]);
    // A removed edge is still the graph's: degrees, and so radii, stay.
    assert_eq!(scene.gpu.radius, before.radius);

    scene.apply(&cut.invert()).unwrap();
    scene.apply(&hide.invert()).unwrap();
    assert_eq!(scene.gpu, before);
    assert!(!scene.drawn.highlight.any_hidden());
}

#[test]
fn degrees_count_distinct_neighbours_as_edges_come_and_go() {
    let mut scene = Scene::new(kite());
    let before = scene.gpu.clone();
    // A second 0-1 edge, a loop, and 1-3 twice: only 1-3 is a new neighbour.
    let delta = growth(5, 0, &[(1, 0), (2, 2), (1, 3), (3, 1)]);
    let changes = scene.apply(&delta).unwrap();
    assert_eq!(scene.live.degrees.degree, [3, 3, 3, 4, 1]);
    assert_eq!(changes.restyled, vec![1..4]);
    // One of the two 1-3 edges goes: still neighbours.
    let last = GraphDelta::new(vec![Op::PopEdges(EdgeBatch::of_label(0, vec![3], vec![1]))]);
    let changes = scene.apply(&last).unwrap();
    assert!(changes.restyled.is_empty());
    assert_eq!(changes.upload_bytes(), 0);
    assert_eq!(scene.live.degrees.degree, [3, 3, 3, 4, 1]);
    scene.apply(&last.invert()).unwrap();
    scene.apply(&delta.invert()).unwrap();
    assert_eq!(scene.gpu, before);
    assert!(scene.live.degrees.added.is_empty());
}

#[test]
fn taking_off_edges_the_view_started_with_recounts_the_degrees() {
    let mut scene = Scene::new(kite());
    let before = scene.gpu.clone();
    let tail = GraphDelta::new(vec![Op::PopEdges(EdgeBatch::of_label(
        0,
        vec![0, 3],
        vec![2, 4],
    ))]);
    let changes = scene.apply(&tail).unwrap();
    assert_eq!(changes.edges, (6, 4));
    assert_eq!(scene.live.degrees.degree, [2, 2, 2, 2, 0]);
    assert_eq!(scene.live.degrees.base_edges, 4);
    scene.apply(&tail.invert()).unwrap();
    assert_eq!(scene.gpu, before);
    // And on from there, counted from the new start.
    scene.apply(&growth(5, 1, &[(5, 0), (4, 3)])).unwrap();
    assert_eq!(scene.live.degrees.degree, [4, 2, 3, 3, 1, 1]);
}

/// The kite with a `rank` to size by and a `kind` to colour by.
fn styled() -> GraphResult {
    let mut r = kite();
    let rank = Column::new("rank", None, Values::F32(vec![1.0, 2.0, 3.0, 4.0, 5.0]));
    let kind = Column::new(
        "kind",
        None,
        Values::dict([Some("a"), Some("b"), None, Some("a"), Some("c")]),
    );
    let ops = vec![
        r.set_column_op(Target::Nodes, rank),
        Op::SetColumn {
            target: Target::Nodes,
            index: 1,
            old: None,
            new: Some(kind),
        },
    ];
    r.apply(&GraphDelta::new(ops)).unwrap();
    let encodings = Encodings {
        size_by: Some("rank".into()),
        color_by: Some("kind".into()),
        ..Encodings::default()
    };
    r.apply(&GraphDelta::new(vec![r.set_encodings_op(encodings)]))
        .unwrap();
    r
}

#[test]
fn encodings_size_and_colour_the_nodes_and_the_legend_counts_them() {
    let scene = Scene::new(styled());
    let d = &scene.drawn;
    let (lo, hi) = SIZE_BY_RADII;
    assert_eq!(d.style.radius[0], lo);
    assert_eq!(d.style.radius[4], hi);
    assert!(d.style.radius[1] > lo && d.style.radius[1] < d.style.radius[2]);
    let palette = &scene.theme.palette;
    assert_eq!(
        d.style.color,
        [
            palette[0],
            palette[1],
            scene.theme.node,
            palette[0],
            palette[2]
        ]
    );
    assert_eq!(d.categories, Some(vec![0, 1, NO_CATEGORY, 0, 2]));
    let info = scene.live.info(&scene.theme);
    assert_eq!(info.size.column.as_deref(), Some("rank"));
    assert_eq!(info.size.range, Some((1.0, 5.0)));
    let legend = info.color.unwrap();
    assert_eq!(legend.column, "kind");
    let entries: Vec<(&str, usize)> = legend
        .entries
        .iter()
        .map(|e| (e.label.as_str(), e.count))
        .collect();
    assert_eq!(entries, [("a", 2), ("b", 1), ("c", 1)]);
    assert_eq!(legend.entries[0].color, "#2a78d6");
    assert_eq!((legend.other, legend.none), (0, 1));
}

#[test]
fn a_new_node_inside_the_range_restyles_no_other_and_one_outside_restyles_all() {
    let mut scene = Scene::new(styled());
    let before = scene.gpu.clone();
    let kind = scene.result().node_column("kind").unwrap().values.clone();
    let add = |rank: f32, kind_text: &str| {
        GraphDelta::new(vec![Op::AddNodes(
            NodeBatch::of_label(0, [5i64])
                .with_column("rank", BatchValues::F32(vec![rank]))
                .with_column("kind", BatchValues::dict(&kind, [Some(kind_text)])),
        )])
    };
    // Rank 2.5 is inside 1..5, and "d" is a new category: the fourth colour.
    let inside = add(2.5, "d");
    let changes = scene.apply(&inside).unwrap();
    assert!(changes.restyled.is_empty());
    assert_eq!(changes.upload_bytes(), 16);
    assert_eq!(scene.drawn.style.color[5], scene.theme.palette[3]);
    let legend = scene.live.info(&scene.theme).color.unwrap();
    assert_eq!(legend.entries.len(), 4);
    assert_eq!(legend.entries[3].label, "d");
    scene.apply(&inside.invert()).unwrap();
    assert_eq!(scene.gpu, before);
    assert_eq!(
        scene.live.info(&scene.theme).color.unwrap().entries.len(),
        3
    );

    // Rank 9 widens the range to 1..9: every node but the least shrinks.
    let outside = add(9.0, "a");
    let changes = scene.apply(&outside).unwrap();
    assert_eq!(changes.restyled, vec![1..5]);
    assert_eq!(scene.live.info(&scene.theme).size.range, Some((1.0, 9.0)));
    assert_eq!(scene.drawn.style.radius[5], SIZE_BY_RADII.1);
    scene.apply(&outside.invert()).unwrap();
    assert_eq!(scene.gpu, before);
    assert_eq!(scene.live.info(&scene.theme).size.range, Some((1.0, 5.0)));
}

#[test]
fn a_changed_column_writes_only_the_nodes_it_changed() {
    let mut scene = Scene::new(styled());
    let before = scene.gpu.clone();
    // Node 2 gets a kind, and node 4 changes its own.
    let kind = Column::new(
        "kind",
        None,
        Values::dict([Some("a"), Some("b"), Some("b"), Some("a"), Some("a")]),
    );
    let set = GraphDelta::new(vec![scene.result().set_column_op(Target::Nodes, kind)]);
    let changes = scene.apply(&set).unwrap();
    assert_eq!(changes.restyled, vec![2..5]);
    assert_eq!(changes.upload_bytes(), 3 * 8);
    let palette = &scene.theme.palette;
    assert_eq!(scene.drawn.style.color[2], palette[1]);
    assert_eq!(scene.drawn.style.color[4], palette[0]);

    // A column nothing is drawn by changes nothing drawn.
    let note = Column::new("note", None, Values::U32(vec![7; 5]));
    let other = GraphDelta::new(vec![scene.result().set_column_op(Target::Nodes, note)]);
    assert!(scene.apply(&other).unwrap().is_empty());

    // No encodings: back to degree and the node colour.
    let plain = GraphDelta::new(vec![scene.result().set_encodings_op(Encodings::default())]);
    let changes = scene.apply(&plain).unwrap();
    assert_eq!(changes.restyled, vec![0..5]);
    assert_eq!(scene.drawn.categories, None);
    assert_eq!(scene.drawn.style.radius[4], NodeStyle::degree_radius(1));

    for delta in [plain, other, set] {
        scene.apply(&delta.invert()).unwrap();
    }
    assert_eq!(scene.gpu, before);
}

#[test]
fn highlights_and_restored_positions_follow_a_delta() {
    let mut scene = Scene::new(kite());
    let before = scene.gpu.clone();
    let route = Highlights {
        paths: vec![Path {
            nodes: vec![1, 0, 3, 4],
            edges: vec![0, 3, 5],
        }],
        ..Highlights::default()
    };
    let light = GraphDelta::new(vec![scene.result().set_highlights_op(route)]);
    let changes = scene.apply(&light).unwrap();
    assert!(changes.highlights);
    assert_eq!(changes.upload_bytes(), 0);
    let flags = scene.drawn.highlight.flags();
    assert_eq!(flags[0], flag::HIGHLIGHTED);
    assert_eq!(flags[1], flag::HIGHLIGHTED | flag::SEED);
    assert_eq!(flags[2], 0);
    assert_eq!(scene.drawn.highlight.edges(), [0, 1, 3, 0, 3, 4]);
    // A removed edge of the path is not drawn highlighted.
    let cut = GraphDelta::new(vec![Op::RemoveEdges(vec![3])]);
    assert!(scene.apply(&cut).unwrap().highlights);
    assert_eq!(scene.drawn.highlight.edges(), [0, 1, 3, 4]);

    // A step that moved every node, as an undo's positions do.
    let there = Positions {
        x: vec![1.0, 2.0, 3.0, 4.0, 5.0],
        y: vec![5.0, 4.0, 3.0, 2.0, 1.0],
    };
    let here = Positions {
        x: scene.drawn.x.clone(),
        y: scene.drawn.y.clone(),
    };
    // The result has no positions of its own: give it the drawn ones first.
    let pin = GraphDelta::new(vec![scene.result().set_positions_op(Some(here))]);
    scene.apply(&pin).unwrap();
    let step = GraphDelta::new(vec![scene.result().set_positions_op(Some(there))]);
    let changes = scene.apply(&step).unwrap();
    assert!(changes.positions);
    assert_eq!(changes.upload_bytes(), 5 * 8);
    assert_eq!(scene.drawn.y, [5.0, 4.0, 3.0, 2.0, 1.0]);

    for delta in [step, pin, cut, light] {
        scene.apply(&delta.invert()).unwrap();
    }
    assert_eq!(scene.gpu, before);
}

#[test]
fn a_delta_that_does_not_fit_changes_nothing() {
    let mut scene = Scene::new(styled());
    let before = scene.gpu.clone();
    let bytes = scene.result().to_bytes();
    // The second op names a node that is not there: the first is undone.
    let bad = GraphDelta::new(vec![
        Op::AddNodes(NodeBatch::of_label(0, [5i64])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![5], vec![9])),
    ]);
    let error = scene
        .live
        .apply(&bad, &mut scene.drawn.parts(&scene.theme))
        .unwrap_err();
    assert!(error.to_string().contains("names node 9"), "{error}");
    assert_eq!(scene.result().to_bytes(), bytes);
    assert!(scene.drawn.highlight.take_dirty().is_empty());
    scene.check();
    assert_eq!(scene.gpu, before);
}

#[test]
fn a_delta_may_take_slots_off_and_fill_them_again() {
    let mut scene = Scene::new(kite());
    scene.apply(&growth(5, 2, &[(4, 5), (5, 6)])).unwrap();
    let before = scene.gpu.clone();
    // Off come nodes 5 and 6 and their edges; on go three others.
    let swap = GraphDelta::new(vec![
        Op::PopEdges(EdgeBatch::of_label(0, vec![4, 5], vec![5, 6])),
        Op::PopNodes(NodeBatch::of_label(0, [5i64, 6])),
        Op::AddNodes(NodeBatch::of_label(0, [7i64, 8, 9])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0, 5, 6], vec![5, 6, 7])),
    ]);
    let changes = scene.apply(&swap).unwrap();
    assert_eq!(changes.nodes, (7, 8));
    assert_eq!(changes.edges, (8, 9));
    // The slots from the least count reached are written anew.
    assert_eq!((changes.new_nodes_from, changes.new_edges_from), (5, 6));
    assert_eq!(scene.live.degrees.degree, [4, 2, 3, 3, 1, 2, 2, 1]);
    scene.apply(&swap.invert()).unwrap();
    // The two nodes are back in their slots, placed again by their parents.
    assert_eq!(scene.gpu, before);
}

/// A fixed generator: the same steps every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

#[test]
fn any_run_of_steps_and_undos_draws_what_a_new_view_would() {
    let mut rng = Rng(441);
    let mut scene = Scene::new(styled());
    let first = scene.gpu.clone();
    let mut done: Vec<GraphDelta> = Vec::new();
    let mut next_key = 5i64;
    for step in 0..300 {
        // Undo more often as the history grows, so it stays a few steps deep.
        if !done.is_empty() && rng.below(12) < done.len() {
            let delta = done.pop().unwrap();
            scene.apply(&delta.invert()).unwrap();
            continue;
        }
        let r = scene.result();
        let (n, m) = (r.n_nodes(), r.n_edges());
        let delta = match rng.below(6) {
            // An expansion: a few nodes off one, and edges among them.
            0 | 1 => {
                let count = 1 + rng.below(4);
                let kind = r.node_column("kind").unwrap().values.clone();
                let texts = ["a", "b", "c", "d", "e"];
                let kinds: Vec<Option<&str>> = (0..count)
                    .map(|_| Some(texts[rng.below(5)]).filter(|_| rng.below(5) > 0))
                    .collect();
                let ranks = (0..count).map(|_| rng.below(12) as f32 - 2.0).collect();
                let nodes = NodeBatch::of_label(0, next_key..next_key + count as i64)
                    .with_column("rank", BatchValues::F32(ranks))
                    .with_column("kind", BatchValues::dict(&kind, kinds));
                next_key += count as i64;
                let edges = 1 + rng.below(2 * count);
                let src = (0..edges).map(|_| (n + rng.below(count)) as u32).collect();
                let dst = (0..edges).map(|_| rng.below(n + count) as u32).collect();
                GraphDelta::new(vec![
                    Op::AddNodes(nodes),
                    Op::AddEdges(EdgeBatch::of_label(0, src, dst)),
                ])
            }
            // Edges among the nodes there are.
            2 => {
                let edges = 1 + rng.below(3);
                let src = (0..edges).map(|_| rng.below(n) as u32).collect();
                let dst = (0..edges).map(|_| rng.below(n) as u32).collect();
                GraphDelta::new(vec![Op::AddEdges(EdgeBatch::of_label(0, src, dst))])
            }
            // Hide a node, or show one.
            3 => {
                let v = rng.below(n);
                GraphDelta::new(vec![if r.node_is_removed(v) {
                    Op::RestoreNodes(vec![v as u32])
                } else {
                    Op::RemoveNodes(vec![v as u32])
                }])
            }
            // Remove an edge, or restore one.
            4 => {
                let e = rng.below(m);
                GraphDelta::new(vec![if r.edge_flags()[e] & REMOVED != 0 {
                    Op::RestoreEdges(vec![e as u32])
                } else {
                    Op::RemoveEdges(vec![e as u32])
                }])
            }
            // Size by the other thing.
            _ => {
                let mut encodings = r.encodings().clone();
                encodings.size_by = match encodings.size_by {
                    Some(_) => None,
                    None => Some("rank".into()),
                };
                GraphDelta::new(vec![r.set_encodings_op(encodings)])
            }
        };
        scene
            .apply(&delta)
            .unwrap_or_else(|e| panic!("step {step}: {e}"));
        done.push(delta);
    }
    assert!(scene.result().n_nodes() > 5, "the run grew the graph");
    while let Some(delta) = done.pop() {
        scene.apply(&delta.invert()).unwrap();
    }
    assert_eq!(
        scene.gpu, first,
        "every step undone is the first frame's data"
    );
}
