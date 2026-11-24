//! `GROUP BY` (design §6, #499): a drawing aggregated by a column.
//!
//! `.graph karate GROUP BY club` draws a disc for each club and the ties
//! between clubs, counted, in place of the members and their ties. What
//! groups the nodes is resolved as `COLOR BY` is: a column of `NODES
//! (query)`, a column of the result's own rows, a vertex column, or an
//! algorithm (`louvain`, `connected_components`, `label_propagation`).
//!
//! [`grouped`] makes the result a front end draws:
//!
//! - **its nodes are the groups**, of the node label [`GROUP`], keyed `0`,
//!   `1`, … from the largest (equal ones as first seen). A column named
//!   as `GROUP BY` names what it groups by (`club`, `louvain`) says each
//!   one's value, which its members share, as the shell prints it;
//!   [`MEMBERS`] counts its members and [`INSIDE`] the edges among them. A drawing of more than [`MAX_GROUPS`] groups keeps the
//!   largest, and the rest are one group at the end, `other (N groups)`;
//! - **its edges are the pairs of groups with an edge between them**, one
//!   each, from the lower group to the higher, in order; the edge column
//!   [`COUNT`] says how many of the drawing's edges each stands for,
//!   whichever way they point;
//! - its encodings size the nodes by [`MEMBERS`] and group them by the
//!   column of the groups' values.
//!
//! The edges inside a group are not in the result, and nor are the members:
//! the [`Grouping`] beside it (`GraphView::groups`) knows which nodes each
//! group has, for the explore session to open one (`.expand group:3`,
//! [`super::session`]). An opened group's members are nodes of the same
//! result, of the vertex tables' own labels, with their group's value in
//! the same column and their place in the group's disc in [`X`] and [`Y`]
//! ([`disc_layout`]).

use std::collections::HashMap;

use datafusion::arrow::array::{Array, ArrayRef};
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{layout_fa2_multilevel, MultilevelParams};
use ursa_core::topology::Topology;
use ursa_viz::model::{
    Column as ModelColumn, EdgeBatch, Encodings, GraphDelta, GraphResult, NodeBatch, NodeLabel, Op,
    Provenance, Role, Strings, Target, Values,
};

use super::build::EdgeList;
use super::{key_type_of, plan_err, thousands, GraphView};

/// The most groups drawn each as its own disc: the largest. The rest are
/// one group, "other".
pub const MAX_GROUPS: usize = 2_000;
/// The label of the nodes that are groups. `.expand group:3` opens the
/// group keyed 3.
pub const GROUP: &str = "group";
/// The node column that counts a group's members.
pub const MEMBERS: &str = "members";
/// The node column that counts the edges among a group's members.
pub const INSIDE: &str = "edges inside";
/// The edge column that counts the edges between two groups.
pub const COUNT: &str = "count";
/// The node columns that place a member of an opened group in its group's
/// disc: a point of the unit disc, the group's centre at its origin.
pub const X: &str = "group x";
pub const Y: &str = "group y";
/// The most members a group may have to be opened in place.
pub const MAX_OPEN: usize = 100_000;
/// The rule's name in a grouped result's provenance.
pub const RULE: &str = "groups";
/// What the nodes with no value are grouped as.
const NO_VALUE: &str = "NULL";

/// Which nodes of a drawing each group of a grouped result has: what
/// `GROUP BY` leaves out of the result, kept for opening a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grouping {
    /// What the nodes are grouped by, as typed.
    by: String,
    /// Each group's name: the values of the grouped result's group column,
    /// in order.
    names: Vec<String>,
    /// Group `g`'s members are `order[starts[g]..starts[g + 1]]`: nodes of
    /// the drawing, ascending.
    starts: Vec<u32>,
    order: Vec<u32>,
    /// The drawing's nodes: each one's key, and its label in the grouped
    /// result (empty when every one is of label 1, the only vertex table).
    keys: Strings,
    labels: Vec<u16>,
}

impl Grouping {
    /// What the nodes are grouped by, as typed.
    pub fn by(&self) -> &str {
        &self.by
    }

    /// How many groups there are, "other" among them.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// How many nodes are grouped.
    pub fn n_nodes(&self) -> usize {
        self.order.len()
    }

    /// Group `group`'s name.
    pub fn name(&self, group: usize) -> &str {
        &self.names[group]
    }

    /// Group `group`'s members: nodes of the drawing that was grouped,
    /// ascending.
    pub fn members(&self, group: usize) -> &[u32] {
        &self.order[self.starts[group] as usize..self.starts[group + 1] as usize]
    }

    /// The key of node `node` of the drawing that was grouped.
    pub fn key(&self, node: u32) -> &str {
        self.keys.get(node as usize)
    }

    /// The label of node `node` in the grouped result: its vertex table.
    pub fn label(&self, node: u32) -> u16 {
        self.labels.get(node as usize).copied().unwrap_or(1)
    }
}

/// The legend of a grouped result: `grouped by louvain: 12 groups`. `None`
/// for a result that is not grouped.
pub fn legend(result: &GraphResult, by: &str) -> Option<String> {
    let name = result.encodings().group_by.as_deref()?;
    let Values::Dict { values, .. } = &result.node_column(name)?.values else {
        return None;
    };
    let groups = values.len();
    Some(format!(
        "grouped by {by}: {} group{}",
        thousands(groups),
        if groups == 1 { "" } else { "s" }
    ))
}

/// Whether node `node` of `result` is a group of a grouped result, not a
/// node of a graph.
pub fn is_group(result: &GraphResult, node: usize) -> bool {
    result.encodings().group_by.is_some()
        && result
            .node_label()
            .get(node)
            .is_some_and(|&l| result.node_labels()[l as usize].name == GROUP)
}

/// How many of `result`'s node labels are not vertex tables: 1, the
/// groups' own, if it is grouped.
pub(crate) fn own_labels(result: &GraphResult) -> usize {
    usize::from(
        result.encodings().group_by.is_some()
            && result
                .node_labels()
                .first()
                .is_some_and(|l| l.name == GROUP),
    )
}

fn model(e: ursa_viz::model::ModelError) -> DataFusionError {
    DataFusionError::Plan(format!("the groups cannot be drawn: {e}"))
}

/// `edges` grouped by `values`, one per node in node order (`by` is what
/// they are, as typed): see the [module](self).
pub(crate) fn grouped(
    edges: EdgeList,
    key_type: &DataType,
    graph: Option<String>,
    mut provenance: Provenance,
    by: &str,
    values: &ArrayRef,
) -> Result<GraphView> {
    let n = edges.n as usize;
    if n == 0 {
        return plan_err(format!("GROUP BY {by}: there are no nodes to group"));
    }
    if values.null_count() == values.len() {
        return plan_err(format!(
            "GROUP BY {by}: no node has a value (are the ids the graph's?)"
        ));
    }
    if edges.labels.iter().any(|l| l == GROUP) {
        return plan_err(format!(
            "GROUP BY {by}: the drawing has a vertex table named '{GROUP}', which is what a \
             grouped drawing calls its groups; draw it under another name"
        ));
    }
    // Each category: its name, its count, in the order first seen. The
    // nodes with no value are one.
    let options = FormatOptions::default();
    let shown = ArrayFormatter::try_new(values.as_ref(), &options)?;
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut categories: Vec<(String, u32)> = Vec::new();
    let mut none: Option<u32> = None;
    let mut of: Vec<u32> = Vec::with_capacity(n);
    for i in 0..n {
        let c = if values.is_null(i) {
            *none.get_or_insert_with(|| {
                categories.push((String::new(), 0));
                categories.len() as u32 - 1
            })
        } else {
            let name = shown.value(i).to_string();
            match seen.get(&name) {
                Some(&c) => c,
                None => {
                    let c = categories.len() as u32;
                    seen.insert(name.clone(), c);
                    categories.push((name, 0));
                    c
                }
            }
        };
        categories[c as usize].1 += 1;
        of.push(c);
    }
    // A name no value has, for the nodes with none and for "other".
    let unused = |seen: &HashMap<String, u32>, mut name: String| {
        while seen.contains_key(&name) {
            name.push('\'');
        }
        name
    };
    if let Some(c) = none {
        let name = unused(&seen, NO_VALUE.to_string());
        seen.insert(name.clone(), c);
        categories[c as usize].0 = name;
    }
    // The largest first; equal ones as first seen.
    let distinct = categories.len();
    let mut order: Vec<u32> = (0..distinct as u32).collect();
    order.sort_by(|&a, &b| {
        categories[b as usize]
            .1
            .cmp(&categories[a as usize].1)
            .then(a.cmp(&b))
    });
    let kept = distinct.min(MAX_GROUPS);
    let mut rank = vec![kept as u32; distinct];
    for (r, &c) in order.iter().take(kept).enumerate() {
        rank[c as usize] = r as u32;
    }
    let mut names: Vec<String> = order[..kept]
        .iter()
        .map(|&c| categories[c as usize].0.clone())
        .collect();
    let mut members: Vec<u32> = order[..kept]
        .iter()
        .map(|&c| categories[c as usize].1)
        .collect();
    if distinct > kept {
        let folded = distinct - kept;
        names.push(unused(
            &seen,
            format!("other ({} groups)", thousands(folded)),
        ));
        members.push(
            order[kept..]
                .iter()
                .map(|&c| categories[c as usize].1)
                .sum(),
        );
        provenance.notes.push(format!(
            "the {} largest of {} groups are drawn, and the other {} as one",
            thousands(kept),
            thousands(distinct),
            thousands(folded)
        ));
    }
    drop(seen);
    drop(categories);
    let groups = names.len();
    for g in &mut of {
        *g = rank[*g as usize];
    }

    // Each group's members, ascending.
    let mut starts = vec![0u32; groups + 1];
    for &g in &of {
        starts[g as usize + 1] += 1;
    }
    for g in 0..groups {
        starts[g + 1] += starts[g];
    }
    let mut next = starts.clone();
    let mut by_group = vec![0u32; n];
    for (i, &g) in of.iter().enumerate() {
        by_group[next[g as usize] as usize] = i as u32;
        next[g as usize] += 1;
    }

    // The edges between groups, counted, and those inside each.
    let mut inside = vec![0u32; groups];
    let mut between: HashMap<(u32, u32), u32> = HashMap::new();
    for (&s, &d) in edges.src.iter().zip(&edges.dst) {
        let (a, b) = (of[s as usize], of[d as usize]);
        if a == b {
            inside[a as usize] += 1;
        } else {
            *between.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    let mut pairs: Vec<((u32, u32), u32)> = between.into_iter().collect();
    pairs.sort_unstable();
    let (src, dst): (Vec<u32>, Vec<u32>) = pairs.iter().map(|&(pair, _)| pair).unzip();
    let counts: Vec<u32> = pairs.iter().map(|&(_, count)| count).collect();
    drop(pairs);

    // The result: the groups as nodes of the label `group`, then the
    // drawing's own labels, which an opened group's members are of.
    let key = key_type_of(key_type, &edges.ids);
    let mut ops = vec![Op::AddNodeLabel(NodeLabel::new(
        GROUP,
        ursa_viz::model::KeyType::Int,
    ))];
    let labels = if edges.labels.is_empty() {
        vec![String::new()]
    } else {
        edges.labels.clone()
    };
    ops.extend(
        labels
            .into_iter()
            .map(|name| Op::AddNodeLabel(NodeLabel::new(name, key))),
    );
    ops.push(Op::AddEdgeLabel(String::new()));
    ops.push(Op::AddNodes(NodeBatch::of_label(0, 0..groups as i64)));
    ops.push(Op::AddEdges(EdgeBatch::of_label(0, src, dst)));
    provenance.rule = Some(RULE.to_string());
    let mut result = GraphResult::build(graph, provenance, &GraphDelta::new(ops)).map_err(model)?;
    // The column of the groups' values is named as GROUP BY names them,
    // unless that is one of this module's own columns.
    let column = if [MEMBERS, INSIDE, X, Y].contains(&by) {
        format!("{by} (group)")
    } else {
        by.to_string()
    };
    let columns = [
        (
            Target::Nodes,
            ModelColumn::new(
                column.clone(),
                Some(Role::Category),
                Values::Dict {
                    values: names.iter().collect(),
                    codes: (0..groups as u32).collect(),
                },
            ),
        ),
        (
            Target::Nodes,
            ModelColumn::new(MEMBERS, Some(Role::Metric), Values::U32(members)),
        ),
        (
            Target::Nodes,
            ModelColumn::new(INSIDE, Some(Role::Metric), Values::U32(inside)),
        ),
        (
            Target::Nodes,
            ModelColumn::new(X, None, Values::F32(vec![f32::NAN; groups])),
        ),
        (
            Target::Nodes,
            ModelColumn::new(Y, None, Values::F32(vec![f32::NAN; groups])),
        ),
        (
            Target::Edges,
            ModelColumn::new(COUNT, Some(Role::Metric), Values::U32(counts)),
        ),
    ];
    for (target, column) in columns {
        let op = result.set_column_op(target, column);
        result.apply(&GraphDelta::new(vec![op])).map_err(model)?;
    }
    let op = result.set_encodings_op(Encodings {
        size_by: Some(MEMBERS.to_string()),
        group_by: Some(column),
        ..Encodings::default()
    });
    result.apply(&GraphDelta::new(vec![op])).map_err(model)?;

    let groups = Grouping {
        by: by.to_string(),
        names,
        starts,
        order: by_group,
        keys: edges.ids.iter().collect(),
        // In the result the drawing's labels come after `group`.
        labels: edges.node_label.iter().map(|&l| l + 1).collect(),
    };
    Ok(GraphView {
        result,
        skipped: edges.skipped,
        positions: None,
        groups: Some(groups),
        adjacency: None,
    })
}

/// How far from the middle of its disc the outermost member of a group is
/// put, as a share of the disc's radius: the members are inside it, with
/// room for their own discs.
pub const DISC_FILL: f32 = 0.9;
/// The work a group's layout may take on the group itself, in adjacency
/// entries plus nodes per iteration, times iterations: a tenth of what a
/// graph's own layout takes, for a picture a tenth as large.
const DISC_WORK: f64 = 3.0e6;

/// Where each of `n` nodes goes in the unit disc, by the edges
/// `(src[i], dst[i])` among them: `x` and `y`, one per node.
///
/// The nodes are laid out as a graph is (`layout_fa2`'s coarse-to-fine
/// ForceAtlas2, with its defaults), and then each is moved along its ray
/// from the layout's middle so that the nodes fill the disc evenly: the
/// `k`-th of them from the middle (ties by index) is put
/// `DISC_FILL * sqrt((k + 0.5) / n)` from it. Neighbours stay beside each
/// other, and no node is outside the disc whatever the layout's shape. One
/// node is at the middle.
///
/// The same nodes and edges give the same bits, on any number of threads.
pub fn disc_layout(n: usize, src: Vec<u32>, dst: Vec<u32>) -> (Vec<f32>, Vec<f32>) {
    if n <= 1 {
        return (vec![0.0; n], vec![0.0; n]);
    }
    let topology = Topology::build(n, src, dst);
    let params = MultilevelParams {
        finest_work: DISC_WORK,
        ..MultilevelParams::default()
    };
    let (mut x, mut y) = layout_fa2_multilevel(&topology, None, params);
    // The middle: of the box, which no node's place depends on the order
    // of a sum for.
    let (mut lo, mut hi) = (
        (f32::INFINITY, f32::INFINITY),
        (f32::NEG_INFINITY, f32::NEG_INFINITY),
    );
    for i in 0..n {
        if x[i].is_finite() && y[i].is_finite() {
            lo = (lo.0.min(x[i]), lo.1.min(y[i]));
            hi = (hi.0.max(x[i]), hi.1.max(y[i]));
        }
    }
    let (cx, cy) = if lo.0 <= hi.0 {
        ((lo.0 + hi.0) / 2.0, (lo.1 + hi.1) / 2.0)
    } else {
        (0.0, 0.0)
    };
    let mut far: Vec<(f32, u32)> = (0..n)
        .map(|i| {
            let (dx, dy) = (x[i] - cx, y[i] - cy);
            let d = (dx * dx + dy * dy).sqrt();
            (if d.is_finite() { d } else { f32::INFINITY }, i as u32)
        })
        .collect();
    far.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    for (k, &(d, i)) in far.iter().enumerate() {
        let i = i as usize;
        let r = DISC_FILL * ((k as f32 + 0.5) / n as f32).sqrt();
        // A node at the middle, or nowhere, goes along the x axis.
        let (ux, uy) = if d > 0.0 && d.is_finite() {
            ((x[i] - cx) / d, (y[i] - cy) / d)
        } else {
            (1.0, 0.0)
        };
        x[i] = r * ux;
        y[i] = r * uy;
    }
    (x, y)
}
