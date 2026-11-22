//! A scene and its renderer together: what a platform binding drives.
//!
//! The binding owns the window or canvas, the device and the frame loop; it
//! calls [`View::step`] to advance the layout and [`View::draw`] to draw a
//! frame into a texture. `web` is one such binding; a native window or a
//! notebook widget would be others.

use ursa_core::algo::{fa2_seed_positions, Fa2Params};

use crate::highlight::{flag, BadHighlight, HighlightState, MARK_REACH};
use crate::interact::nearest_edge;
use crate::live::{Changes, Drawn, Info, LiveResult, Parts};
use crate::model::{GraphDelta, GraphResult, ModelError, Op};
use crate::pace::{ready, yield_now, Pace};
use crate::render::{node_radius_scale, starting_capacity, FrontNode, Renderer};
use crate::scene::{bounds, Camera, Graph, Growth, LiveLayout, NodeStyle, Pairs, Theme};
use crate::spatial::{Discs, EdgeGrid, NodeGrid};

pub struct View {
    graph: Graph,
    /// The layout, when it runs on this thread: made on the first
    /// [`View::step`]. A view whose layout runs elsewhere (a Web Worker) gets
    /// its positions through [`View::set_positions`] and never makes one.
    layout: Option<LiveLayout>,
    /// The positions drawn: the layout's, or the last set.
    x: Vec<f32>,
    y: Vec<f32>,
    style: NodeStyle,
    /// The colours drawn with.
    theme: Theme,
    /// Each node's category, once coloured by one ([`View::color_by`]): kept
    /// so a new theme recolours them.
    categories: Option<Vec<u32>>,
    /// Each node's flags, and the highlighted edges.
    highlight: HighlightState,
    /// The nodes drawn in front ([`flag::FRONT`], and not hidden), in order.
    front: Vec<u32>,
    camera: Camera,
    renderer: Renderer,
    /// Fit the camera to the layout as it moves, until the user pans or zooms.
    follow: bool,
    /// The camera's zoom when last fitted: nodes grow back toward full size
    /// as the user zooms in past it.
    fitted_zoom: f32,
    /// Draw nodes as well as edges.
    pub draw_nodes: bool,
    /// The result drawn, for a view made from one ([`View::from_result`]):
    /// what [`View::apply`] changes.
    live: Option<LiveResult>,
    /// The graph changed since the layout on this thread last saw it.
    layout_stale: bool,
    /// The box around the positions as last fitted, while the camera
    /// follows and nothing has moved since: a delta that only adds nodes
    /// widens it by theirs, without reading every position again.
    fitted: Option<Fitted>,
    /// Where the nodes are, for picking and culling ([`Spatial`]).
    spatial: Spatial,
}

/// The grids over the positions, and how far they can be trusted: the
/// positions move every frame while a layout runs, and a grid is built
/// only once they have stopped.
#[derive(Debug, Default)]
struct Spatial {
    /// The grid over the nodes, as last built.
    grid: Option<NodeGrid>,
    /// The nodes before this one are where the grid has them. The rest have
    /// moved since, or are new: a pick tests each of them.
    fresh: usize,
    /// Frames and picks since a position last changed.
    quiet: u32,
    /// At least every node's radius, in pixels.
    max_radius: f32,
    /// The grid over the edges, built for the second edge picked with the
    /// positions at rest: building it costs a few picks that read every
    /// edge, which one stray click should not pay.
    edges: Option<EdgeGrid>,
    /// Edges picked since the positions came to rest.
    edge_picks: u32,
    /// Whether the nodes in view have been worked out for the scene and
    /// camera as they are ([`View::prepare`]).
    culled: bool,
}

/// A grid is built once this many frames and picks have passed with no
/// position changed. While a layout runs that never happens (a frame, a
/// hover, and the positions change again), unless its steps are slower
/// than the frames, when a grid costs less than a step and is used until
/// the next.
const QUIET: u32 = 3;

/// The most edges an edge grid is built over (four bytes an edge): past
/// it, picking an edge reads every edge.
const EDGE_GRID_MAX: usize = 1 << 22;

/// A graph with fewer nodes draws them all, in view or not.
const CULL_MIN_NODES: usize = 1024;

/// The most nodes in view that are drawn alone: they are uploaded, 20
/// bytes each, when the camera moves.
const CULL_MAX_NODES: usize = 1 << 16;

fn max_radius(radius: &[f32]) -> f32 {
    radius.iter().copied().fold(0.0, f32::max)
}

type Bounds = Option<(f32, f32, f32, f32)>;

/// The bounds the camera is fitted to.
#[derive(Debug, Clone, Default)]
struct Fitted {
    bounds: Bounds,
    /// The bounds before each delta that added nodes since, with the node
    /// count it had: taking those nodes off again is the bounds before.
    before: Vec<(usize, Bounds)>,
}

/// What a new view draws.
enum Scene {
    Graph(Graph),
    Result(Box<GraphResult>),
}

/// How many edges go to the GPU at a time when a view is made: half a
/// megabyte of indexes.
const EDGE_RUN: usize = 1 << 16;

/// What [`View::apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The slots the delta changed.
    pub changes: Changes,
    /// Bytes written to the GPU for it, everything counted: nodes, edges,
    /// flags, and the highlight layer's instances.
    pub bytes_uploaded: u64,
    /// Whether a buffer grew to hold it (its contents copied on the GPU).
    pub grew: bool,
}

impl View {
    /// A view of `graph` drawn into `format` textures of `width` x `height`
    /// pixels. Nodes are sized by degree until [`View::set_style`].
    ///
    /// Making a view costs an upload, not a graph build: the degrees are
    /// counted from the edge list and the nodes start where a layout would
    /// seed them, so no adjacency and no layout is made until
    /// [`View::step`] asks for one (a view whose layout runs elsewhere
    /// never does), and the edges go to the GPU a run at a time, never
    /// held a second time as a list of indexes.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        graph: Graph,
        width: u32,
        height: u32,
    ) -> Self {
        let scene = Scene::Graph(graph);
        let mut pace = Pace::never();
        ready(Self::make(
            device, queue, format, scene, width, height, &mut pace,
        ))
    }

    /// [`View::new`], giving the event loop a turn between its pieces
    /// ([`Pace::yielding`]): what a page calls, so that making the view
    /// never holds its thread for long.
    pub async fn new_paced(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        graph: Graph,
        width: u32,
        height: u32,
    ) -> Self {
        let scene = Scene::Graph(graph);
        let mut pace = Pace::yielding();
        Self::make(device, queue, format, scene, width, height, &mut pace).await
    }

    /// A view of `result`: its nodes where it puts them (or where a layout
    /// would start them), sized and coloured by its encodings (by degree,
    /// without), its paths and seeds highlighted and its removed nodes
    /// hidden. [`View::apply`] then changes it in place. Like
    /// [`View::new`], it builds no adjacency and no layout.
    pub fn from_result(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        result: GraphResult,
        width: u32,
        height: u32,
    ) -> Self {
        let scene = Scene::Result(Box::new(result));
        let mut pace = Pace::never();
        ready(Self::make(
            device, queue, format, scene, width, height, &mut pace,
        ))
    }

    /// [`View::from_result`], giving the event loop a turn between its
    /// pieces, as [`View::new_paced`] does.
    pub async fn from_result_paced(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        result: GraphResult,
        width: u32,
        height: u32,
    ) -> Self {
        let scene = Scene::Result(Box::new(result));
        let mut pace = Pace::yielding();
        Self::make(device, queue, format, scene, width, height, &mut pace).await
    }

    async fn make(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        scene: Scene,
        width: u32,
        height: u32,
        pace: &mut Pace,
    ) -> Self {
        let theme = Theme::light();
        pace.breathe().await;
        // The graph, and its degrees from the edge list, a pass at a time.
        let (graph, result) = match scene {
            Scene::Graph(graph) => (graph, None),
            Scene::Result(mut result) => (LiveResult::graph_of(&mut result), Some(result)),
        };
        pace.breathe().await;
        let mut counted = Pairs::count(&graph);
        pace.breathe().await;
        counted.fill(&graph);
        pace.breathe().await;
        let (pairs, degrees) = counted.finish();
        pace.breathe().await;
        let (live, drawn) = match result {
            None => {
                drop(pairs);
                let style = NodeStyle::by_degree(&degrees);
                // The seeded positions, as a layout starts from them.
                let (x, y) = fa2_seed_positions(graph.n_nodes(), &Fa2Params::default());
                let highlight = HighlightState::new(graph.n_nodes());
                let drawn = Drawn {
                    graph,
                    x,
                    y,
                    style,
                    categories: None,
                    highlight,
                };
                (None, drawn)
            }
            Some(result) => {
                let (live, drawn) = LiveResult::with_graph(*result, graph, pairs, degrees, &theme);
                (Some(live), drawn)
            }
        };
        let Drawn {
            graph,
            x,
            y,
            style,
            categories,
            highlight,
        } = drawn;
        let (n, m) = (graph.n_nodes(), graph.n_edges());
        // A view that takes deltas has room for the first, so that they do
        // not grow the buffers.
        let room = match live {
            Some(_) => (starting_capacity(n), starting_capacity(m)),
            None => (n, m),
        };
        let renderer = Renderer::new_paced(device, format, &x, &y, &style, m, room, pace).await;
        // The edges, a run at a time: each is in the GPU's hands, and its
        // staging freed, before the next is made.
        let mut start = 0;
        while start < m {
            let run = start..m.min(start + EDGE_RUN);
            let lines = match &live {
                Some(live) => live.line_indices(run.clone()),
                None => graph.line_indices(run.clone()),
            };
            renderer.write_edges(queue, start, &lines);
            queue.submit([]);
            let _ = device.poll(wgpu::PollType::Poll);
            start = run.end;
            pace.breathe().await;
        }
        let mut view = Self {
            graph,
            layout: None,
            x,
            y,
            style,
            theme,
            categories,
            highlight,
            front: Vec::new(),
            camera: Camera::new(width as f32, height as f32),
            renderer,
            follow: true,
            fitted_zoom: 1.0,
            draw_nodes: true,
            live,
            layout_stale: false,
            fitted: None,
            spatial: Spatial::default(),
        };
        view.spatial.max_radius = max_radius(&view.style.radius);
        if view.live.is_some() {
            view.upload_flags(device, queue, false);
        }
        view.fit();
        view
    }

    /// The result drawn, with every delta applied, if the view was made
    /// from one.
    pub fn result(&self) -> Option<&GraphResult> {
        self.live.as_ref().map(LiveResult::result)
    }

    /// What is shown, for a status line and a legend, if the view was made
    /// from a result ([`LiveResult::info`]).
    pub fn info(&self) -> Option<Info> {
        self.live.as_ref().map(|live| live.info(&self.theme))
    }

    /// Apply a delta to the scene in place, in time proportional to the
    /// delta: new nodes and edges are written after the ones there are (the
    /// buffers grow by doubling), removed nodes are hidden by their flag
    /// and removed edges by their slot, and only the styles that changed
    /// are written. Undo is the inverse delta ([`GraphDelta::invert`]): no
    /// index changes, and the frame after a delta and its inverse is the
    /// frame before, bit for bit.
    ///
    /// The view must have been made from a result ([`View::from_result`]).
    /// If the delta does not fit the result, nothing changes.
    pub fn apply(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        delta: &GraphDelta,
    ) -> Result<Applied, ModelError> {
        self.apply_at(device, queue, delta, None)
    }

    /// [`View::apply`], and then draw every node at `positions` (`x` and
    /// `y`, one per node after the delta): an undo, with the positions
    /// saved before its step ([`View::positions`]).
    pub fn apply_at(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        delta: &GraphDelta,
        positions: Option<(&[f32], &[f32])>,
    ) -> Result<Applied, ModelError> {
        let Some(live) = self.live.as_mut() else {
            return Err(ModelError(
                "this view was not made from a result, so it has none to apply a delta to".into(),
            ));
        };
        if let Some((x, y)) = positions {
            let mut n = self.graph.n_nodes();
            for op in &delta.ops {
                match op {
                    Op::AddNodes(batch) => n += batch.len(),
                    Op::PopNodes(batch) => n = n.saturating_sub(batch.len()),
                    _ => {}
                }
            }
            if x.len() != n || y.len() != n {
                return Err(ModelError(format!(
                    "{} x and {} y positions for the {n} nodes there are after the delta",
                    x.len(),
                    y.len()
                )));
            }
        }
        let mut changes = live.apply(
            delta,
            &mut Parts {
                graph: &mut self.graph,
                x: &mut self.x,
                y: &mut self.y,
                style: &mut self.style,
                categories: &mut self.categories,
                highlight: &mut self.highlight,
                theme: &self.theme,
            },
        )?;
        if let Some((x, y)) = positions {
            self.x.copy_from_slice(x);
            self.y.copy_from_slice(y);
            changes.positions = true;
        }
        let ((n0, n1), (m0, m1)) = (changes.nodes, changes.edges);
        // The grid still has the nodes that were there and did not move.
        let spatial = &mut self.spatial;
        spatial.fresh = match changes.positions {
            true => 0,
            false => spatial.fresh.min(changes.new_nodes_from).min(n1),
        };
        spatial.quiet = 0;
        spatial.edges = None;
        spatial.edge_picks = 0;
        let restyled = changes.restyled.iter().cloned();
        for run in restyled.chain(std::iter::once(changes.new_nodes_from.min(n1)..n1)) {
            let most = max_radius(&self.style.radius[run]);
            spatial.max_radius = spatial.max_radius.max(most);
        }
        spatial.culled = false;
        self.renderer.set_culled_nodes_off();
        let grows = self.renderer.grows();
        self.renderer.resize(device, queue, n1, m1);

        let from = changes.new_nodes_from;
        let (radius, color) = (&self.style.radius, &self.style.color);
        let mut bytes = 0;
        if changes.positions {
            self.renderer.set_positions(queue, &self.x, &self.y);
            bytes += 8 * n1 as u64;
            bytes += self
                .renderer
                .write_styles(queue, from, &radius[from..], Some(&color[from..]));
        } else {
            bytes += self.renderer.write_nodes(
                queue,
                from,
                &self.x[from..],
                &self.y[from..],
                &radius[from..],
                &color[from..],
            );
        }
        for run in &changes.restyled {
            let color = changes.recolored.then(|| &color[run.clone()]);
            bytes += self
                .renderer
                .write_styles(queue, run.start, &radius[run.clone()], color);
        }
        let new_edges = changes.new_edges_from..m1;
        let runs = std::iter::once(&new_edges).chain(&changes.edge_runs);
        for run in runs {
            let lines = live.line_indices(run.clone());
            bytes += self.renderer.write_edges(queue, run.start, &lines);
        }
        if changes.highlights {
            bytes += self.upload_flags(device, queue, true);
        } else {
            // No flag changed: the new nodes' are all there is to write,
            // and nothing reads every node's.
            let new = self.highlight.take_unsent();
            let flags = &self.highlight.flags()[new.clone()];
            bytes += self.renderer.write_flags(queue, new.start, flags);
            // The highlight layer holds positions, radii and colours of
            // its own.
            if changes.positions || !changes.restyled.is_empty() {
                let layer = |r: &Renderer| r.highlight_bytes_uploaded() + r.front_bytes_uploaded();
                let before = layer(&self.renderer);
                self.upload_highlight(device, queue, changes.positions, true);
                bytes += layer(&self.renderer) - before;
            }
        }

        if (n0, m0) != (n1, m1) {
            self.layout_stale = true;
        }
        if n0 != n1 || changes.positions {
            self.refit(&changes);
        }
        Ok(Applied {
            changes,
            bytes_uploaded: bytes,
            grew: self.renderer.grows() != grows,
        })
    }

    /// Upload the flags that changed and, if they did or `layer` says so,
    /// the highlight layer's instances. Returns the bytes written.
    fn upload_flags(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, layer: bool) -> u64 {
        let uploaded = |r: &Renderer| {
            r.flag_bytes_uploaded() + r.highlight_bytes_uploaded() + r.front_bytes_uploaded()
        };
        let before = uploaded(&self.renderer);
        let dirty = self.highlight.take_dirty();
        self.renderer.set_flags(queue, &self.highlight, &dirty);
        self.uncull();
        if layer || !dirty.is_empty() {
            self.front = self.highlight.front_nodes().collect();
            self.upload_highlight(device, queue, true, true);
        }
        uploaded(&self.renderer) - before
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn camera(&self) -> &Camera {
        &self.camera
    }

    /// Layout iterations run on this thread so far.
    pub fn iterations(&self) -> u32 {
        self.layout.as_ref().map_or(0, LiveLayout::iterations)
    }

    pub fn positions(&self) -> (&[f32], &[f32]) {
        (&self.x, &self.y)
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Run `iterations` more layout iterations and upload the new positions.
    pub fn step(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, iterations: u32) {
        if iterations == 0 {
            return;
        }
        let graph = &self.graph;
        if std::mem::take(&mut self.layout_stale) {
            // The graph changed under the layout ([`View::apply`]): grow it,
            // or, if nodes went, start again from what is drawn.
            if let Some(mut layout) = self.layout.take() {
                let fits = layout.positions().0.len() <= graph.n_nodes();
                self.layout = if fits && layout.grow(graph, Growth::default()).is_ok() {
                    Some(layout)
                } else {
                    let (x, y) = (self.x.clone(), self.y.clone());
                    LiveLayout::from_positions(graph, Fa2Params::default(), x, y).ok()
                };
            }
        }
        let layout = self
            .layout
            .get_or_insert_with(|| LiveLayout::new(graph, Fa2Params::default()));
        layout.step(iterations);
        let (x, y) = layout.positions();
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
        self.upload(device, queue);
    }

    /// Draw these positions (one per node), from a layout run elsewhere.
    pub fn set_positions(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        x: &[f32],
        y: &[f32],
    ) {
        assert_eq!(x.len(), self.graph.n_nodes());
        assert_eq!(y.len(), self.graph.n_nodes());
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
        self.upload(device, queue);
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.renderer.set_positions(queue, &self.x, &self.y);
        // Every node moved: no grid has them now.
        self.spatial.fresh = 0;
        self.spatial.quiet = 0;
        self.spatial.edges = None;
        self.spatial.edge_picks = 0;
        self.uncull();
        // The highlighted edges and the nodes in front hold positions.
        self.upload_highlight(device, queue, true, true);
        self.fitted = None;
        if self.follow {
            self.fit_camera();
        }
    }

    /// Each node's flags ([`flag`]) and the highlighted edges.
    pub fn highlight(&self) -> &HighlightState {
        &self.highlight
    }

    /// Change the flags or the highlighted edges, and upload what changed:
    /// the flags of the nodes that changed (four bytes each), the
    /// highlighted edges (16 bytes each) and the nodes in front (20 bytes
    /// each). The graph's edges, positions and styles are not uploaded. On
    /// an error nothing changes.
    pub fn set_highlight(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        change: impl FnOnce(&mut HighlightState) -> Result<(), BadHighlight>,
    ) -> Result<(), BadHighlight> {
        let mut next = self.highlight.clone();
        change(&mut next)?;
        let flags_changed = next.flags() != self.highlight.flags();
        // Hiding a node takes its highlighted edges with it.
        let edges_changed = next.edges() != self.highlight.edges()
            || next.selection_edges() != self.highlight.selection_edges()
            || flags_changed && (next.any_hidden() || self.highlight.any_hidden());
        self.highlight = next;
        let dirty = self.highlight.take_dirty();
        self.renderer.set_flags(queue, &self.highlight, &dirty);
        self.uncull();
        if flags_changed {
            self.front = self.highlight.front_nodes().collect();
        }
        self.upload_highlight(device, queue, edges_changed, flags_changed);
        Ok(())
    }

    /// Write the highlighted edges, the nodes in front, or both, as they
    /// are now. Nothing is written for a set that was and stays empty.
    fn upload_highlight(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        edges: bool,
        front: bool,
    ) {
        if edges && !(!self.highlight.any_wide_edges() && self.renderer.highlight_edges() == 0) {
            let ends = self.highlight_ends();
            self.renderer.set_highlight_edges(device, queue, &ends);
        }
        if front && !(self.front.is_empty() && self.renderer.front_nodes() == 0) {
            let flags = self.highlight.flags();
            let nodes: Vec<FrontNode> = self
                .front
                .iter()
                .map(|&i| {
                    let i = i as usize;
                    FrontNode::new(
                        self.x[i],
                        self.y[i],
                        self.style.radius[i],
                        self.style.color[i],
                        flags[i],
                    )
                })
                .collect();
            self.renderer.set_front_nodes(device, queue, &nodes);
        }
    }

    /// The highlight and the hidden nodes of `result`
    /// ([`HighlightState::set_result`]): its paths and seeds, and its
    /// removed nodes. `result`'s nodes must be the graph's, in order.
    pub fn set_result_highlights(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        result: &GraphResult,
    ) -> Result<(), BadHighlight> {
        self.set_highlight(device, queue, |h| h.set_result(result))
    }

    /// The highlighted edges to draw, each as its two ends' positions.
    fn highlight_ends(&self) -> Vec<[f32; 4]> {
        let (x, y) = (&self.x, &self.y);
        self.highlight
            .shown_edges()
            .map(|(s, d)| {
                let (s, d) = (s as usize, d as usize);
                [x[s], y[s], x[d], y[d]]
            })
            .collect()
    }

    /// Where node `i` is drawn, in pixels from the canvas's top left.
    pub fn node_pixel(&self, i: usize) -> (f32, f32) {
        self.camera.to_pixel(self.x[i], self.y[i])
    }

    pub fn set_style(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, style: NodeStyle) {
        assert_eq!(style.radius.len(), self.graph.n_nodes());
        assert_eq!(style.color.len(), self.graph.n_nodes());
        self.renderer.set_style(queue, &style);
        self.spatial.max_radius = max_radius(&style.radius);
        self.style = style;
        self.uncull();
        // The nodes in front hold their radius and colour.
        self.upload_highlight(device, queue, false, true);
    }

    pub fn style(&self) -> &NodeStyle {
        &self.style
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Colour nodes by category, one per node, with the theme's palette;
    /// `None` draws every node in the theme's node colour.
    pub fn color_by(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        categories: Option<Vec<u32>>,
    ) {
        if let Some(c) = &categories {
            assert_eq!(c.len(), self.graph.n_nodes());
        }
        self.categories = categories;
        self.recolor(device, queue);
    }

    /// Draw with `theme`: its background, edge ink and highlight colours,
    /// and node colours from its palette (or its node colour).
    pub fn set_theme(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, theme: Theme) {
        self.renderer.set_theme(&theme);
        self.theme = theme;
        self.recolor(device, queue);
    }

    fn recolor(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut style = self.style.clone();
        style.color = match &self.categories {
            Some(c) => self.theme.colors(c),
            None => vec![self.theme.node; self.graph.n_nodes()],
        };
        self.set_style(device, queue, style);
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.camera.viewport = (width.max(1) as f32, height.max(1) as f32);
        self.uncull();
        if self.follow {
            self.fit_camera();
        }
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.follow = false;
        self.camera.pan(dx, dy);
        self.uncull();
    }

    pub fn zoom(&mut self, factor: f32, px: f32, py: f32) {
        self.follow = false;
        self.camera.zoom(factor, px, py);
        self.uncull();
    }

    /// Fit the whole layout in view, and keep it fitted as it moves.
    pub fn fit(&mut self) {
        self.follow = true;
        self.fit_camera();
    }

    fn fit_camera(&mut self) {
        let b = bounds(&self.x, &self.y);
        self.fit_to(b);
        self.fitted = Some(Fitted {
            bounds: b,
            before: Vec::new(),
        });
    }

    fn fit_to(&mut self, bounds: Bounds) {
        if let Some(b) = bounds {
            self.camera.fit(b);
            self.fitted_zoom = self.camera.pixels_per_unit;
            self.uncull();
        }
    }

    /// Fit the camera after a delta that changed the nodes or moved them,
    /// if it follows. The bounds are those of every position, as
    /// [`View::fit_camera`] finds them, but found from the last fit's where
    /// the delta only added nodes or took the last additions off: a box is
    /// the same whichever order its points are added in.
    fn refit(&mut self, changes: &Changes) {
        let (n0, n1) = changes.nodes;
        let fitted = match self.fitted.take() {
            Some(fitted) if self.follow && !changes.positions => fitted,
            _ => {
                // Not following, or everything moved: start again.
                if self.follow {
                    self.fit_camera();
                }
                return;
            }
        };
        let Fitted {
            bounds: was,
            mut before,
        } = fitted;
        let added = changes.new_nodes_from == n0 && n1 > n0;
        let undone = n1 < n0 && changes.new_nodes_from == n1;
        let now = if added {
            before.push((n0, was));
            let new = bounds(&self.x[n0..], &self.y[n0..]);
            match (was, new) {
                (Some(a), Some(b)) => {
                    Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
                }
                (a, b) => a.or(b),
            }
        } else if undone && before.last().is_some_and(|b| b.0 == n1) {
            before.pop().expect("it has a last").1
        } else {
            self.fit_camera();
            return;
        };
        self.fit_to(now);
        self.fitted = Some(Fitted {
            bounds: now,
            before,
        });
    }

    /// The nodes as a pick sees them.
    fn discs(&self) -> Discs<'_> {
        Discs {
            x: &self.x,
            y: &self.y,
            radius: &self.style.radius,
            flags: self.highlight.flags(),
            camera: &self.camera,
            scale: self.radius_scale(),
        }
    }

    /// Whether the grid has every node where it is now.
    fn grid_is_fresh(&self) -> bool {
        let n = self.x.len();
        (self.spatial.grid.as_ref()).is_some_and(|g| g.len() == n && self.spatial.fresh == n)
    }

    /// A frame or a pick with the positions as they were: once there have
    /// been [`QUIET`] of them, the positions are at rest, and the grid is
    /// built if it is behind.
    fn rested(&mut self) {
        self.spatial.quiet = self.spatial.quiet.saturating_add(1);
        if self.spatial.quiet >= QUIET && !self.grid_is_fresh() {
            self.rebuild_grid();
        }
    }

    /// Build the grid over the nodes now, wherever they are: one pass over
    /// the positions (see `BENCH.md`). The view does it itself once the
    /// positions rest; this is for a caller that knows they have, and for
    /// timing it.
    pub fn rebuild_grid(&mut self) {
        self.spatial.grid = Some(NodeGrid::build(&self.x, &self.y));
        self.spatial.fresh = self.x.len();
        self.uncull();
    }

    /// Whether the grid has every node where it is now: a pick reads only
    /// the nodes near the pointer, and a zoomed-in frame draws only the
    /// nodes in view.
    pub fn grid_ready(&self) -> bool {
        self.grid_is_fresh()
    }

    /// The node drawn under the pixel `(px, py)`, if any: the nearest node
    /// whose disc covers it and, of nodes equally near, the first. A hidden
    /// node is not there to pick.
    ///
    /// It reads the nodes in the grid's cells around the pixel, and every
    /// node that moved or was added since the grid was built: all of them
    /// while a layout runs. The answer is the same either way
    /// ([`View::pick_linear`]).
    pub fn pick(&mut self, px: f32, py: f32) -> Option<u32> {
        self.rested();
        let discs = self.discs();
        match &self.spatial.grid {
            Some(grid) => discs.pick(grid, self.spatial.fresh, self.spatial.max_radius, px, py),
            None => discs.pick_linear(px, py),
        }
    }

    /// [`View::pick`], testing every node: what the grid must agree with.
    pub fn pick_linear(&self, px: f32, py: f32) -> Option<u32> {
        self.discs().pick_linear(px, py)
    }

    /// Whether edge `e` is not drawn: it was removed, or a node it joins is
    /// hidden.
    fn edge_is_gone(&self, e: u32) -> bool {
        let flags = self.highlight.flags();
        let hidden = |v: u32| flags.get(v as usize).is_none_or(|f| f & flag::HIDDEN != 0);
        let (src, dst) = (self.graph.src(), self.graph.dst());
        hidden(src[e as usize])
            || hidden(dst[e as usize])
            || (self.live.as_ref()).is_some_and(|l| !l.result().edge_is_shown(e as usize))
    }

    /// The edge drawn nearest the pixel `(px, py)`, within `tolerance`
    /// pixels of it: its index. Of edges equally near, the first. An edge
    /// of a hidden node is not there to pick, nor is a removed edge.
    ///
    /// With the positions at rest it reads the edges near the pixel, from a
    /// grid of the edges built for the second such pick (up to 4M edges);
    /// otherwise every edge. The answer is the same either way
    /// ([`View::pick_edge_linear`]).
    pub fn pick_edge(&mut self, px: f32, py: f32, tolerance: f32) -> Option<u32> {
        self.rested();
        let (src, dst) = (self.graph.src(), self.graph.dst());
        let at_rest = self.grid_is_fresh() && src.len() <= EDGE_GRID_MAX;
        if at_rest && self.spatial.edges.is_none() {
            self.spatial.edge_picks += 1;
            if self.spatial.edge_picks >= 2 {
                self.spatial.edges = Some(EdgeGrid::build(src, dst, &self.x, &self.y));
            }
        }
        let Some(grid) = self.spatial.edges.as_ref().filter(|_| at_rest) else {
            return self.pick_edge_linear(px, py, tolerance);
        };
        let at = self.camera.to_world(px, py);
        let reach = tolerance / self.camera.pixels_per_unit;
        let skip = |e| self.edge_is_gone(e);
        grid.nearest(src, dst, &self.x, &self.y, skip, at, reach)
    }

    /// [`View::pick_edge`], testing every edge.
    pub fn pick_edge_linear(&self, px: f32, py: f32, tolerance: f32) -> Option<u32> {
        let (src, dst) = (self.graph.src(), self.graph.dst());
        let at = self.camera.to_world(px, py);
        let reach = tolerance / self.camera.pixels_per_unit;
        let skip = |e| self.edge_is_gone(e);
        nearest_edge(src, dst, &self.x, &self.y, skip, at, reach)
    }

    /// The camera, a position, a style or a flag changed: the nodes in
    /// view are worked out again before the next frame.
    fn uncull(&mut self) {
        self.spatial.culled = false;
        self.renderer.set_culled_nodes_off();
    }

    /// Get ready to draw a frame: call it before [`View::draw`]. Once the
    /// positions rest it builds the grid, and when under half the nodes
    /// (and at most 65,536) are in view it gives the renderer those alone
    /// to draw, 20 bytes each, uploaded when the camera or a node changes.
    /// The frame is the same, bit for bit: only nodes that put nothing on
    /// the canvas are left out, and the rest are drawn in the same order.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.rested();
        if std::mem::replace(&mut self.spatial.culled, true) {
            return;
        }
        if let Some(nodes) = self.nodes_in_view() {
            let flags = self.highlight.flags();
            let nodes: Vec<FrontNode> = nodes
                .into_iter()
                .map(|i| {
                    let i = i as usize;
                    let (radius, color) = (self.style.radius[i], self.style.color[i]);
                    FrontNode::new(self.x[i], self.y[i], radius, color, flags[i])
                })
                .collect();
            self.renderer.set_culled_nodes(device, queue, Some(&nodes));
        }
    }

    /// The nodes the pass over every node draws on the canvas, in index
    /// order, if they are few enough to draw alone and the grid has every
    /// node. A hidden node, and a node in front (drawn in its own pass),
    /// is not among them.
    fn nodes_in_view(&self) -> Option<Vec<u32>> {
        let n = self.x.len();
        if n < CULL_MIN_NODES || !self.grid_is_fresh() {
            return None;
        }
        let grid = self.spatial.grid.as_ref()?;
        let discs = self.discs();
        // A node's quad reaches this far from its centre, and a little is
        // added for the rounding on the way to the canvas.
        let reach = self.spatial.max_radius * discs.scale + 1.0 + MARK_REACH;
        if !reach.is_finite() {
            return None;
        }
        let (w, h) = self.camera.viewport;
        let (w, h) = (f64::from(w), f64::from(h));
        let reach = f64::from(reach) * 1.0001 + 1e-4 * (w + h) + 1e-3;
        let cells = discs.cells_in(grid, -reach, -reach, w + reach, h + reach);
        let count = grid.count(cells);
        if 2 * count >= n || count > CULL_MAX_NODES {
            return None;
        }
        let flags = self.highlight.flags();
        let apart = flag::HIDDEN | flag::FRONT;
        let mut nodes: Vec<u32> = grid
            .runs(cells)
            .flatten()
            .copied()
            .filter(|&i| flags[i as usize] & apart == 0)
            .collect();
        nodes.sort_unstable();
        Some(nodes)
    }

    /// How many nodes the pass over every node draws: all of them, or the
    /// nodes in view ([`View::prepare`]).
    pub fn nodes_drawn(&self) -> usize {
        self.renderer.nodes_drawn() as usize
    }

    /// Draw a frame offscreen and read it back: `width * height` RGBA pixels,
    /// row by row from the top. `format` is the format the view draws in.
    /// For an image of the view, and for tests that cannot see the canvas.
    pub async fn snapshot(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Result<Vec<u8>, wgpu::BufferAsyncError> {
        let (width, height) = (self.camera.viewport.0 as u32, self.camera.viewport.1 as u32);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("snapshot"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.draw(
            device,
            queue,
            &texture.create_view(&wgpu::TextureViewDescriptor::default()),
        );
        // Rows are padded to 256 bytes for the copy.
        let row = width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("snapshot"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("snapshot"),
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let (tx, mut rx) = futures_channel::oneshot::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| drop(tx.send(r)));
        // The map completes during a poll (WebGL2, native) or on its own
        // (WebGPU): poll, and give the event loop a turn, until it has.
        let result = loop {
            let _ = device.poll(wgpu::PollType::Poll);
            match rx.try_recv() {
                Ok(Some(r)) => break r,
                Ok(None) => yield_now().await,
                Err(_) => break Err(wgpu::BufferAsyncError),
            }
        };
        result?;
        let data = buffer.slice(..).get_mapped_range();
        let bgra = matches!(
            format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut out = Vec::with_capacity((row * height) as usize);
        for r in 0..height as usize {
            let line = &data[r * padded as usize..r * padded as usize + row as usize];
            if bgra {
                for px in line.chunks_exact(4) {
                    out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            } else {
                out.extend_from_slice(line);
            }
        }
        drop(data);
        buffer.unmap();
        Ok(out)
    }

    /// How much nodes are shrunk now: enough that, fitted, they cover at most
    /// 8% of the canvas, easing back to full size as the view zooms in.
    pub fn radius_scale(&self) -> f32 {
        let (w, h) = self.camera.viewport;
        let fitted = node_radius_scale(&self.style.radius, w, h, 0.08);
        let zoomed_in = (self.camera.pixels_per_unit / self.fitted_zoom).max(1.0);
        (fitted * zoomed_in.sqrt()).min(1.0)
    }

    /// Draw a frame into `target`.
    pub fn draw(&self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::TextureView) {
        self.renderer.draw(
            device,
            queue,
            target,
            &self.camera,
            self.draw_nodes,
            self.radius_scale(),
        );
    }
}
