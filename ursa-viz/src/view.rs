//! A scene and its renderer together: what a platform binding drives.
//!
//! The binding owns the window or canvas, the device and the frame loop; it
//! calls [`View::step`] to advance the layout and [`View::draw`] to draw a
//! frame into a texture. `web` is one such binding; a native window or a
//! notebook widget would be others.
//!
//! A binding with a frame loop draws with [`View::plan_frame`] and
//! [`View::draw_planned`] (or [`View::draw_frame`], which is both), and
//! calls [`View::refine`] between frames while [`View::refining`]: what a
//! frame cannot draw whole is then drawn in slices once the view rests
//! ([`crate::refine`]).

use std::cell::Cell;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ursa_core::algo::{fa2_seed_positions, Fa2Params};
use ursa_core::topology::UndirectedCsr;

use crate::budget::{split, total_units, Frame, FrameBudget};
use crate::highlight::{flag, BadHighlight, HighlightState, MARK_REACH};
use crate::interact::nearest_edge;
use crate::live::{Changes, Drawn, Info, LiveResult, Parts};
use crate::model::{GraphDelta, GraphResult, ModelError, Op, Room, Source, UgrError};
use crate::order::{check_order_from, BadOrder, EdgeOrder, Ordered};
use crate::pace::{ready, yield_now, Pace};
use crate::refine::{change, Change, Kind, SlicePace, SLICE_TIMEOUT_MS};
use crate::render::{
    disc_area, radius_scale_for, starting_capacity, FrontNode, Renderer, Slice, Under,
};
use crate::scene::{bounds, Camera, Graph, Growth, LiveLayout, NodeStyle, Pairs, Theme};
use crate::spatial::{Discs, EdgeGrid, NodeGrid};

mod level;

pub struct View {
    /// The graph, for a view made of one ([`View::new`]). A view made from
    /// a result ([`View::from_result`]) has none: the result (`live`) is
    /// the one holder of its nodes and edges, and they are read from it.
    graph: Option<Graph>,
    /// A result's undirected adjacency, for a layout on this thread: built
    /// on the first [`View::step`], and again after a delta. A graph keeps
    /// its own ([`Graph::adjacency`]).
    adjacency: Option<Arc<UndirectedCsr>>,
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
    /// Pixels a fit leaves free at each side ([`View::set_fit_padding`]).
    fit_padding: f32,
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
    /// How many edges a frame draws while the view moves.
    budget: FrameBudget,
    /// How many edges are drawn in importance order
    /// ([`View::set_edge_order`]): the first ones.
    ordered: Ordered,
    /// An order being written to the GPU ([`View::order_edges`]).
    reorder: Option<Reorder>,
    /// The nodes a moving frame draws when it cannot draw them all.
    some: SomeNodes,
    /// What the last moving frame drew: nodes and edges.
    moving_drew: (usize, usize),
    /// Where the nodes are, for picking and culling ([`Spatial`]).
    spatial: Spatial,
    /// A level of the layout's hierarchy, drawn in place of the graph
    /// until the layout reaches the graph itself ([`View::set_level`]).
    level: Option<level::Level>,
    /// The detail texture: what it holds, and the refinement that fills
    /// it ([`crate::refine`]).
    detail: Detail,
}

/// The detail texture's side of a view.
#[derive(Default)]
struct Detail {
    /// What the texture holds, if anything of use.
    held: Option<Held>,
    /// A refinement is to begin at the next [`View::refine`].
    wanted: bool,
    /// Something a frame shows has changed since the last frame drawn.
    stale: bool,
    /// How large the slices are; `None` before the first.
    pace: Option<SlicePace>,
    /// The last refinement that finished.
    last: Option<Refinement>,
    /// Never refine: a frame at rest draws everything at once
    /// ([`View::set_refining`]).
    off: bool,
}

/// What the detail texture holds: everything under the highlighted edges
/// as `camera` saw it, or the first slices of that.
struct Held {
    camera: Camera,
    key: Key,
    /// Whether the rest was dimmed, and any node hidden, when it was begun.
    dims: bool,
    hidden: bool,
    progress: Progress,
    /// The first slice, which clears the texture, has been sent.
    cleared: bool,
    /// The edges, and the instances of the node pass, sent so far; and how
    /// many instances there are.
    edges: usize,
    nodes: usize,
    node_total: usize,
    /// The slices the GPU has not finished, oldest first: at most
    /// [`SLICES_AHEAD`].
    pending: VecDeque<Pending>,
    /// When the GPU was last seen to finish a slice, or was sent one with
    /// none before it: the slice it is on began then.
    busy_since: f64,
    began_ms: f64,
    slices: u32,
    longest_ms: f64,
    /// Once done: every node's flags as the texture has it drawn, and the
    /// nodes it has no copy of that the node pass now draws.
    flags: Vec<u8>,
    missing: BTreeSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    /// Slices are being drawn.
    Refining,
    /// Something stopped them: what is there is a part, good for under a
    /// moving frame and no more.
    Stopped,
    /// Everything is there.
    Done,
}

/// How many slices the GPU is sent ahead of the one it is on: one, so
/// that it has the next when it finishes, and is not idle until the next
/// call of [`View::refine`] finds it has. A pan or a zoom waits for those
/// two at most.
const SLICES_AHEAD: usize = 2;

/// A slice sent to the GPU.
struct Pending {
    /// Set when the GPU has finished it.
    done: Arc<AtomicBool>,
    sent_ms: f64,
    /// What it drew, how many, and whether that was a slice's full size.
    kind: Option<Kind>,
    drawn: usize,
    whole: bool,
}

/// What, besides the camera, the texture's pixels depend on and nothing
/// tells the view of when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    radius_scale: u32,
    draw_nodes: bool,
    counts: (usize, usize),
}

/// A refinement that finished ([`FrameInfo::refinement`]).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refinement {
    /// From its first slice's being sent to its last's being done, in
    /// milliseconds.
    pub ms: f64,
    /// How many slices it took.
    pub slices: u32,
    /// The longest the GPU took over the slices one call of
    /// [`View::refine`] found done (one, or two), in milliseconds, as the
    /// calls saw it.
    pub longest_slice_ms: f64,
    /// The edges and the nodes in its last full slices.
    pub slice_edges: usize,
    pub slice_nodes: usize,
}

/// What a frame is to draw ([`View::plan_frame`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Planned {
    /// How much, and whether the view moves. `partial` is whether what is
    /// on the canvas after it lacks anything.
    pub frame: Frame,
    how: How,
}

impl Planned {
    /// Whether there is anything to draw: if not, the canvas is left as it
    /// is ([`View::draw_planned`] draws nothing), and no texture need be
    /// had for it.
    pub fn draws(&self) -> bool {
        self.how != How::Nothing
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum How {
    /// The canvas stays as it is while the slices are drawn.
    Nothing,
    /// Everything, at once.
    Everything,
    /// A level of the layout's hierarchy, in place of the graph
    /// ([`View::set_level`]): what its plan gathered of it.
    Level,
    /// The detail texture, which has everything.
    Refined,
    /// What fits: so many edges and nodes, over the detail texture as it
    /// was drawn for another camera if there is one.
    Some {
        edges: usize,
        some_nodes: Option<usize>,
        under: Option<Camera>,
    },
}

/// The nodes for a frame that does not draw them all: the largest first.
#[derive(Default)]
struct SomeNodes {
    /// Every node, by radius, largest first (then by index); empty until
    /// a frame needs it, and after the nodes or their radii change.
    order: Vec<u32>,
    /// The renderer's copy of the first of them is as they are now.
    fresh: bool,
}

/// An edge order on its way to the GPU: the frames draw the buffer there
/// was until the new one is whole.
struct Reorder {
    /// `order[slot]` is the edge to draw in that slot: read a run at a
    /// time, from wherever it is held.
    order: Box<dyn EdgeOrder>,
    /// The new buffer, and how many of its slots are written.
    buffer: wgpu::Buffer,
    written: usize,
}

/// What the frame budget is doing, for a status line and for tests
/// ([`View::frame_info`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameInfo {
    /// The edges the last moving frame drew: the prefix. 0 before any.
    pub k: usize,
    /// The nodes the last moving frame drew: the largest. 0 before any.
    pub nodes: usize,
    /// The edges a frame at rest draws: every slot.
    pub edges: usize,
    /// How many of them are in importance order ([`View::set_edge_order`]):
    /// the first ones. The rest are after them, in index order.
    pub ordered: usize,
    /// The 95th percentile of the last 120 moving frames' costs, in
    /// milliseconds, as the binding measured them ([`View::frame_cost`]);
    /// `None` before any.
    pub p95: Option<f32>,
    /// How many moving frames' costs were measured.
    pub frames: usize,
    /// The cost a moving frame is held to, in milliseconds.
    pub limit: f64,
    /// Whether slices of the frame at rest remain to be drawn
    /// ([`View::refining`]).
    pub refining: bool,
    /// Whether the detail texture has everything, as the camera sees it
    /// now: a frame at rest draws it.
    pub refined: bool,
    /// The last refinement that finished; `None` before any.
    pub refinement: Option<Refinement>,
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
    /// The area of every node's disc, until a radius changes: a frame and
    /// a pick both need it, and it is a pass over every node.
    disc_area: Cell<Option<f32>>,
    /// The grid over the edges, built for the second edge picked with the
    /// positions at rest: building it costs a few picks that read every
    /// edge, which one stray click should not pay.
    edges: Option<EdgeGrid>,
    /// Edges picked since the positions came to rest.
    edge_picks: u32,
    /// Whether the nodes in view have been worked out for the scene and
    /// camera as they are ([`View::prepare`]).
    culled: bool,
    /// Never draw the nodes in view alone ([`View::set_culling`]).
    no_culling: bool,
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

/// The room a view's result is decoded with: [`crate::live::room`] for its
/// first deltas.
fn room_of(nodes: usize, edges: usize) -> Room {
    Room {
        nodes: crate::live::room(nodes),
        edges: crate::live::room(edges),
    }
}

/// The most turns of the event loop [`settle`] waits for the GPU: a GPU
/// that slow is waited for no longer, and the view is made holding more.
const SETTLE_TURNS: usize = 32;

/// For a paced view: give the event loop turns until the GPU has taken
/// everything written to it, so that the staging of those writes is freed.
async fn settle(device: &wgpu::Device, pace: &mut Pace) {
    if !pace.yields() {
        return;
    }
    for _ in 0..SETTLE_TURNS {
        pace.turn().await;
        if device
            .poll(wgpu::PollType::Poll)
            .map_or(true, |status| status.is_queue_empty())
        {
            return;
        }
    }
}

/// What a new view draws.
enum Scene {
    Graph(Graph),
    Result(Box<GraphResult>),
}

/// How many edges go to the GPU at a time when a view is made: half a
/// megabyte of indexes.
const EDGE_RUN: usize = 1 << 16;

/// How many bytes of edges a paced view writes to the GPU before it gives
/// the event loop a turn, so that the writes' staging is freed: half a
/// megabyte, a run.
const EDGE_STAGING: usize = 1 << 19;

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

    /// [`View::from_result_paced`] of a result still in its wire format
    /// (UGR), wherever those bytes are ([`Source`]): each section is copied
    /// once, from the source to where the result keeps it, with room for
    /// the view's first deltas, and decoding gives the event loop its turns
    /// as making the view does. So a page's thread is not held while a
    /// large result is read, and the buffer is never in this program's
    /// memory beside the result made of it.
    pub async fn from_ugr_paced(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        ugr: &dyn Source,
        width: u32,
        height: u32,
    ) -> Result<Self, UgrError> {
        let mut pace = Pace::yielding();
        // The header says how many nodes and edges to make room for; the
        // sections are read with it.
        let result = GraphResult::read_paced(ugr, &room_of, &mut pace).await?;
        let scene = Scene::Result(Box::new(result));
        Ok(Self::make(device, queue, format, scene, width, height, &mut pace).await)
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
        // The graph, or the result that holds it, and its degrees from the
        // edge list, a pass at a time.
        let (graph, mut result) = match scene {
            Scene::Graph(graph) => (Some(graph), None),
            Scene::Result(result) => (None, Some(result)),
        };
        if let Some(result) = &mut result {
            LiveResult::make_room(result);
            pace.breathe().await;
        }
        let (pairs, degrees) = {
            let (n, src, dst) = match (&graph, &result) {
                (Some(graph), _) => (graph.n_nodes(), graph.src(), graph.dst()),
                (None, Some(result)) => (result.n_nodes(), result.src(), result.dst()),
                (None, None) => unreachable!("a scene is a graph or a result"),
            };
            let mut counted = Pairs::count(n, src, dst);
            pace.breathe().await;
            counted.fill(src, dst);
            pace.breathe().await;
            counted.finish()
        };
        pace.breathe().await;
        let (live, drawn) = match result {
            None => {
                drop(pairs);
                let n = graph.as_ref().map_or(0, Graph::n_nodes);
                let style = NodeStyle::by_degree(&degrees);
                // The seeded positions, as a layout starts from them.
                let (x, y) = fa2_seed_positions(n, &Fa2Params::default());
                let highlight = HighlightState::new(n);
                let drawn = Drawn {
                    x,
                    y,
                    style,
                    categories: None,
                    highlight,
                };
                (None, drawn)
            }
            Some(result) => {
                let (live, drawn) = LiveResult::with_pairs(*result, pairs, degrees, &theme);
                (Some(live), drawn)
            }
        };
        let Drawn {
            x,
            y,
            style,
            categories,
            highlight,
        } = drawn;
        let (n, m) = match (&graph, &live) {
            (Some(graph), _) => (graph.n_nodes(), graph.n_edges()),
            (None, Some(live)) => (live.result().n_nodes(), live.result().n_edges()),
            (None, None) => unreachable!("a scene is a graph or a result"),
        };
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
        let mut staged = 0;
        while start < m {
            let run = start..m.min(start + EDGE_RUN);
            let lines = match (&live, &graph) {
                (Some(live), _) => live.line_indices(run.clone()),
                (None, Some(graph)) => graph.line_indices(run.clone()),
                (None, None) => unreachable!("a scene is a graph or a result"),
            };
            renderer.write_edges(queue, start, &lines);
            queue.submit([]);
            let _ = device.poll(wgpu::PollType::Poll);
            start = run.end;
            // What is written is staged in this program's memory until
            // the GPU has taken it, which on WebGL2 it does between the
            // page's tasks, in its own time: wait for it after each run,
            // or all the runs are held at once.
            staged += lines.len() * 4;
            if staged >= EDGE_STAGING {
                staged = 0;
                settle(device, pace).await;
            } else {
                pace.breathe().await;
            }
        }
        let mut view = Self {
            graph,
            adjacency: None,
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
            fit_padding: 0.0,
            draw_nodes: true,
            live,
            layout_stale: false,
            fitted: None,
            budget: FrameBudget::new(),
            ordered: Ordered::default(),
            reorder: None,
            some: SomeNodes::default(),
            moving_drew: (0, 0),
            spatial: Spatial::default(),
            level: None,
            detail: Detail::default(),
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
        // An order on its way to the GPU is written out first: the delta
        // changes the buffer it fills.
        self.order_edges(device, queue, usize::MAX);
        let Some(live) = self.live.as_mut() else {
            return Err(ModelError(
                "this view was not made from a result, so it has none to apply a delta to".into(),
            ));
        };
        if let Some((x, y)) = positions {
            let mut n = live.result().n_nodes();
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
        // Whatever changed, the detail texture does not have it.
        self.detail.wanted = false;
        self.detail.held = None;
        self.detail.stale = true;
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
        if n0 != n1 || changes.new_nodes_from < n1 || !changes.restyled.is_empty() {
            spatial.disc_area.set(None);
        }
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
        let runs = if self
            .ordered
            .keeps(changes.new_edges_from, &changes.edge_runs)
        {
            // New edges go after the ones there are, in the slots of their
            // indexes, whatever the order of the rest; and so are the
            // edges added since the order that change now.
            std::iter::once(new_edges).chain(changes.edge_runs.iter().cloned())
        } else {
            // An ordered edge changed, or ordered edges are gone (an undo
            // past them): the view does not know its slot
            // ([`crate::order`]), so every edge goes back to the slot of
            // its index.
            self.ordered = Ordered::default();
            std::iter::once(0..m1).chain([].iter().cloned())
        };
        for run in runs {
            let mut start = run.start;
            while start < run.end {
                let piece = start..run.end.min(start + EDGE_RUN);
                let lines = live.line_indices(piece.clone());
                bytes += self.renderer.write_edges(queue, start, &lines);
                start = piece.end;
            }
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
            self.adjacency = None;
        }
        if changes.positions {
            self.budget.moved();
        }
        // The nodes, or their sizes, may have changed.
        self.some = SomeNodes::default();
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
        self.flags_changed(device, queue, &dirty);
        if layer || !dirty.is_empty() {
            self.front = self.highlight.front_nodes().collect();
            self.upload_highlight(device, queue, true, true);
        }
        uploaded(&self.renderer) - before
    }

    /// How many nodes there are: every slot, hidden or not.
    pub fn n_nodes(&self) -> usize {
        match (&self.graph, &self.live) {
            (Some(graph), _) => graph.n_nodes(),
            (None, Some(live)) => live.result().n_nodes(),
            (None, None) => 0,
        }
    }

    /// How many edges there are: every slot, removed or not.
    pub fn n_edges(&self) -> usize {
        self.edges().0.len()
    }

    /// The edges, `(src[i], dst[i])` as node indexes: the graph's, or the
    /// result's for a view made from one, which holds them once.
    pub fn edges(&self) -> (&[u32], &[u32]) {
        match (&self.graph, &self.live) {
            (Some(graph), _) => (graph.src(), graph.dst()),
            (None, Some(live)) => (live.result().src(), live.result().dst()),
            (None, None) => (&[], &[]),
        }
    }

    /// Whether an adjacency has been built on this thread: only
    /// [`View::step`] builds one, so a view whose layout runs elsewhere
    /// never has.
    pub fn has_adjacency(&self) -> bool {
        self.adjacency.is_some() || self.graph.as_ref().is_some_and(Graph::has_adjacency)
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

    /// The renderer, to change how it draws (the edges' opacity, the dim
    /// level): the detail texture is drawn again.
    pub fn renderer_mut(&mut self) -> &mut Renderer {
        self.forget_detail();
        self.detail.stale = true;
        &mut self.renderer
    }

    /// Draw nodes as well as edges, or the edges alone.
    pub fn set_draw_nodes(&mut self, on: bool) {
        if self.draw_nodes != on {
            self.draw_nodes = on;
            self.forget_detail();
            self.detail.stale = true;
        }
    }

    /// Run `iterations` more layout iterations and upload the new positions.
    pub fn step(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, iterations: u32) {
        if iterations == 0 {
            return;
        }
        // The adjacency: the graph's own, or built here from the result's
        // edges (every slot, as a graph of them would have).
        let adjacency = match (&self.graph, &self.live) {
            (Some(graph), _) => Arc::clone(graph.adjacency()),
            (None, Some(live)) => {
                let r = live.result();
                Arc::clone(self.adjacency.get_or_insert_with(|| {
                    Arc::new(UndirectedCsr::from_edges(r.n_nodes(), r.src(), r.dst()))
                }))
            }
            (None, None) => return,
        };
        let n = adjacency.offsets.len() - 1;
        if std::mem::take(&mut self.layout_stale) {
            // The graph changed under the layout ([`View::apply`]): grow it,
            // or, if nodes went, start again from what is drawn.
            if let Some(mut layout) = self.layout.take() {
                let fits = layout.positions().0.len() <= n;
                let grown = fits
                    && layout
                        .grow_on(Arc::clone(&adjacency), Growth::default())
                        .is_ok();
                self.layout = if grown {
                    Some(layout)
                } else {
                    let (x, y) = (self.x.clone(), self.y.clone());
                    let params = Fa2Params::default();
                    LiveLayout::from_positions_on(Arc::clone(&adjacency), params, x, y).ok()
                };
            }
        }
        let layout = self
            .layout
            .get_or_insert_with(|| LiveLayout::new_on(adjacency, Fa2Params::default()));
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
        assert_eq!(x.len(), self.n_nodes());
        assert_eq!(y.len(), self.n_nodes());
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
        self.upload(device, queue);
    }

    /// [`View::set_positions`] for a caller whose positions are not in a
    /// slice here (a page's array): `fill(x, y)` writes every node's into
    /// the positions the view keeps, in place, and they are uploaded from
    /// there.
    pub fn set_positions_with(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fill: impl FnOnce(&mut [f32], &mut [f32]),
    ) {
        fill(&mut self.x, &mut self.y);
        self.upload(device, queue);
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.moved();
        self.forget_detail();
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
        self.flags_changed(device, queue, &dirty);
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
        self.detail.stale = true;
        if edges && !(!self.highlight.any_wide_edges() && self.renderer.highlight_edges() == 0) {
            let ends = self.highlight_ends();
            self.renderer.set_highlight_edges(device, queue, &ends);
        }
        if front {
            // Positions, styles or flags changed: the nodes a moving frame
            // draws are gathered again when one next needs them.
            self.some.fresh = false;
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
        assert_eq!(style.radius.len(), self.n_nodes());
        assert_eq!(style.color.len(), self.n_nodes());
        self.renderer.set_style(queue, &style);
        self.spatial.max_radius = max_radius(&style.radius);
        self.spatial.disc_area.set(None);
        self.style = style;
        self.some = SomeNodes::default();
        self.uncull();
        self.forget_detail();
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
            assert_eq!(c.len(), self.n_nodes());
        }
        self.categories = categories;
        self.recolor(device, queue);
    }

    /// Draw with `theme`: its background, edge ink and highlight colours,
    /// and node colours from its palette (or its node colour).
    pub fn set_theme(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, theme: Theme) {
        self.forget_detail();
        self.renderer.set_theme(&theme);
        self.theme = theme;
        self.recolor(device, queue);
    }

    fn recolor(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut style = self.style.clone();
        style.color = match &self.categories {
            Some(c) => self.theme.colors(c),
            None => vec![self.theme.node; self.n_nodes()],
        };
        self.set_style(device, queue, style);
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.moved();
        // The texture is the size the canvas was.
        self.forget_detail();
        self.renderer.release_detail();
        self.camera.viewport = (width.max(1) as f32, height.max(1) as f32);
        self.uncull();
        if self.follow {
            self.fit_camera();
        }
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.moved();
        self.follow = false;
        self.camera.pan(dx, dy);
        self.uncull();
    }

    pub fn zoom(&mut self, factor: f32, px: f32, py: f32) {
        self.moved();
        self.follow = false;
        self.camera.zoom(factor, px, py);
        self.uncull();
    }

    /// Fit the whole layout in view, and keep it fitted as it moves.
    pub fn fit(&mut self) {
        self.follow = true;
        self.fit_camera();
    }

    /// Leave `pixels` free at each side of the canvas when the camera fits
    /// ([`View::fit`], and while it follows): for what is drawn larger
    /// around a node than a node is, as the discs of `GROUP BY`'s groups
    /// are (#499). 0, the default, is the fit as it was.
    pub fn set_fit_padding(&mut self, pixels: f32) {
        self.fit_padding = if pixels > 0.0 { pixels } else { 0.0 };
        if self.follow {
            self.fit_camera();
        }
    }

    fn fit_camera(&mut self) {
        // A level's nodes, while one is drawn: the graph's have no place yet.
        if let Some(b) = self.level_bounds() {
            return self.fit_to(b);
        }
        let b = bounds(&self.x, &self.y);
        self.fit_to(b);
        self.fitted = Some(Fitted {
            bounds: b,
            before: Vec::new(),
        });
    }

    fn fit_to(&mut self, bounds: Bounds) {
        if let Some(b) = bounds {
            self.moved();
            self.camera.fit(b);
            if self.fit_padding > 0.0 {
                // As if the canvas were that much smaller at each side.
                let (w, h) = self.camera.viewport;
                let room = |side: f32| ((side - 2.0 * self.fit_padding) / side).max(0.1);
                let (x0, y0, x1, y1) = b;
                let (bw, bh) = ((x1 - x0).max(1e-6), (y1 - y0).max(1e-6));
                let padded = (w * room(w) / bw).min(h * room(h) / bh) * 0.92;
                self.camera.pixels_per_unit = self.camera.pixels_per_unit.min(padded);
            }
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
        self.recull();
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
        // A level's discs are not the graph's nodes: nothing to pick.
        if self.draws_level() {
            return None;
        }
        self.rested();
        let discs = self.discs();
        match &self.spatial.grid {
            Some(grid) => discs.pick(grid, self.spatial.fresh, self.spatial.max_radius, px, py),
            None => discs.pick_linear(px, py),
        }
    }

    /// [`View::pick`], testing every node: what the grid must agree with.
    pub fn pick_linear(&self, px: f32, py: f32) -> Option<u32> {
        if self.draws_level() {
            return None;
        }
        self.discs().pick_linear(px, py)
    }

    /// Whether edge `e` is not drawn: it was removed, or a node it joins is
    /// hidden.
    fn edge_is_gone(&self, e: u32) -> bool {
        let flags = self.highlight.flags();
        let hidden = |v: u32| flags.get(v as usize).is_none_or(|f| f & flag::HIDDEN != 0);
        let (src, dst) = self.edges();
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
        if self.draws_level() {
            return None;
        }
        self.rested();
        let at_rest = self.grid_is_fresh() && self.n_edges() <= EDGE_GRID_MAX;
        if at_rest && self.spatial.edges.is_none() {
            self.spatial.edge_picks += 1;
            if self.spatial.edge_picks >= 2 {
                let (src, dst) = self.edges();
                let grid = EdgeGrid::build(src, dst, &self.x, &self.y);
                self.spatial.edges = Some(grid);
            }
        }
        let (src, dst) = self.edges();
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
        if self.draws_level() {
            return None;
        }
        let (src, dst) = self.edges();
        let at = self.camera.to_world(px, py);
        let reach = tolerance / self.camera.pixels_per_unit;
        let skip = |e| self.edge_is_gone(e);
        nearest_edge(src, dst, &self.x, &self.y, skip, at, reach)
    }

    /// The camera, a position, a style or a flag changed: the nodes in
    /// view are worked out again before the next frame, which is not the
    /// frame on the canvas.
    fn uncull(&mut self) {
        self.detail.stale = true;
        self.spatial.culled = false;
        self.renderer.set_culled_nodes_off();
    }

    /// The nodes in view are worked out again before the next frame, which
    /// is the same frame: there is a grid to find them with, or they are
    /// no longer to be drawn alone. A refinement begins again: its node
    /// slices are runs of the pass over every node, which is now another
    /// list of the same nodes.
    fn recull(&mut self) {
        self.spatial.culled = false;
        self.renderer.set_culled_nodes_off();
        self.restart_refining();
    }

    /// Get ready to draw a frame: call it before [`View::draw`]. Once the
    /// positions rest it builds the grid, and when under half the nodes
    /// (and at most 65,536) are in view it gives the renderer those alone
    /// to draw, 20 bytes each, uploaded when the camera or a node changes.
    /// The frame is the same, bit for bit: only nodes that put nothing on
    /// the canvas are left out, and the rest are drawn in the same order.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.draws_level() {
            return self.prepare_level(device, queue);
        }
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

    /// Whether a zoomed-in frame may draw the nodes in view alone
    /// ([`View::prepare`]): on unless turned off, which a test does to
    /// compare the two frames.
    pub fn set_culling(&mut self, on: bool) {
        self.spatial.no_culling = !on;
        self.recull();
    }

    /// The nodes the pass over every node draws on the canvas, in index
    /// order, if they are few enough to draw alone and the grid has every
    /// node. A hidden node, and a node in front (drawn in its own pass),
    /// is not among them.
    fn nodes_in_view(&self) -> Option<Vec<u32>> {
        let n = self.x.len();
        if n < CULL_MIN_NODES || self.spatial.no_culling || !self.grid_is_fresh() {
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
        self.read_back(device, queue, format, |target| {
            self.draw(device, queue, target)
        })
        .await
    }

    /// [`View::snapshot`], of the frame a view at rest shows once it is
    /// refined: the detail texture under the highlighted edges and the
    /// nodes in front, as [`View::draw_planned`] draws it. `None` unless
    /// the texture has everything as the camera sees it now
    /// ([`FrameInfo::refined`]). It is the frame of [`View::snapshot`],
    /// byte for byte, until a node comes to the front or leaves it
    /// ([`crate::refine`]).
    pub async fn snapshot_refined(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Option<Result<Vec<u8>, wgpu::BufferAsyncError>> {
        if !self.refined() {
            return None;
        }
        let draw = |target: &wgpu::TextureView| {
            let (camera, scale) = (&self.camera, self.radius_scale());
            self.renderer.draw_over(
                device,
                queue,
                target,
                camera,
                self.draw_nodes,
                scale,
                Under::Detail,
            );
        };
        Some(self.read_back(device, queue, format, draw).await)
    }

    /// Draw into an offscreen texture of the canvas's size and read it
    /// back: RGBA, row by row from the top.
    async fn read_back(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        draw: impl FnOnce(&wgpu::TextureView),
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
        draw(&texture.create_view(&wgpu::TextureViewDescriptor::default()));
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
        let area = match self.spatial.disc_area.get() {
            Some(area) => area,
            None => {
                let area = disc_area(&self.style.radius);
                self.spatial.disc_area.set(Some(area));
                area
            }
        };
        let fitted = radius_scale_for(area, w, h, 0.08);
        let zoomed_in = (self.camera.pixels_per_unit / self.fitted_zoom).max(1.0);
        (fitted * zoomed_in.sqrt()).min(1.0)
    }

    /// Draw the edges in `order`, most important first: `order[slot]` is
    /// the edge to draw in that slot, a permutation of the first
    /// `order.len()` edges (the edges there were when it was computed:
    /// [`LiveLayout::edge_order`]). Edges after those stay after them, in
    /// index order. A frame that draws every edge is the same pixels in any
    /// order; the order is for the frames that draw a prefix
    /// ([`View::draw_frame`]). A delta that removes or restores an ordered
    /// edge, or takes ordered edges off, puts every edge back in index
    /// order ([`crate::order`]).
    ///
    /// Nothing is written here: [`View::order_edges`] writes the edges to
    /// a new buffer, a run at a time, and the frames draw the old one until
    /// it is whole. On an error nothing changes.
    pub fn set_edge_order(
        &mut self,
        device: &wgpu::Device,
        order: Vec<u32>,
    ) -> Result<(), BadOrder> {
        self.set_edge_order_from(device, Box::new(order))
    }

    /// [`View::set_edge_order`] of an order held elsewhere (a page's
    /// array): it is checked and then written a run at a time from there,
    /// and never held here whole.
    pub fn set_edge_order_from(
        &mut self,
        device: &wgpu::Device,
        order: Box<dyn EdgeOrder>,
    ) -> Result<(), BadOrder> {
        check_order_from(&*order, self.n_edges())?;
        self.reorder = Some(Reorder {
            order,
            buffer: self.renderer.edge_buffer(device),
            written: 0,
        });
        Ok(())
    }

    /// Whether an edge order is on its way to the GPU
    /// ([`View::order_edges`] has more to write).
    pub fn is_ordering(&self) -> bool {
        self.reorder.is_some()
    }

    /// Write up to `max_edges` more edges of the order being set
    /// ([`View::set_edge_order`]), eight bytes each, and draw from the new
    /// buffer once it is whole. Returns whether there is more to write: a
    /// page calls it once a task until there is not, so that a million
    /// edges never hold its thread. A delta writes what is left at once.
    pub fn order_edges(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        max_edges: usize,
    ) -> bool {
        let Some(mut reorder) = self.reorder.take() else {
            return false;
        };
        let m = self.n_edges();
        let ordered = reorder.order.len();
        let mut left = max_edges;
        while reorder.written < m && left > 0 {
            let start = reorder.written;
            let end = m.min(start + left.min(EDGE_RUN));
            // The ordered edges, then the rest in the slots of their indexes.
            let lines = if start < ordered {
                let mut run = vec![0u32; end.min(ordered) - start];
                reorder.order.read(start, &mut run);
                self.lines_of(&run)
            } else {
                match (&self.live, &self.graph) {
                    (Some(live), _) => live.line_indices(start..end),
                    (None, Some(graph)) => graph.line_indices(start..end),
                    (None, None) => Vec::new(),
                }
            };
            self.renderer
                .write_edges_to(queue, &reorder.buffer, start, &lines);
            queue.submit([]);
            let _ = device.poll(wgpu::PollType::Poll);
            reorder.written += lines.len() / 2;
            left -= lines.len() / 2;
        }
        if reorder.written < m {
            self.reorder = Some(reorder);
            return true;
        }
        self.renderer.replace_edges(reorder.buffer);
        self.ordered = Ordered(ordered);
        // A slice is a run of the buffer, which is now another.
        self.restart_refining();
        false
    }

    /// The edges `edges` as the renderer draws them, in that order.
    fn lines_of(&self, edges: &[u32]) -> Vec<u32> {
        match &self.live {
            Some(live) => live.lines_of(edges),
            None => {
                let (src, dst) = self.edges();
                edges
                    .iter()
                    .flat_map(|&e| [src[e as usize], dst[e as usize]])
                    .collect()
            }
        }
    }

    /// The frame budget: how many edges a frame draws while the view moves.
    pub fn budget(&self) -> &FrameBudget {
        &self.budget
    }

    /// What the frame budget is doing.
    pub fn frame_info(&self) -> FrameInfo {
        FrameInfo {
            k: self.moving_drew.1,
            nodes: self.moving_drew.0,
            edges: self.renderer.counts().1,
            ordered: self.ordered.0,
            p95: self.budget.p95_ms(),
            frames: self.budget.observed(),
            limit: self.budget.limit_ms(),
            refining: self.refining(),
            refined: self.refined(),
            refinement: self.detail.last,
        }
    }

    /// Draw a frame into `target`, within the frame budget
    /// ([`crate::budget`]): if the camera or the positions changed since
    /// the last frame, or less than [`crate::budget::REST_MS`] before
    /// `now_ms` (any clock, in milliseconds), only as much as the last
    /// frames' costs say fits: the first edges of the buffer and, if the
    /// nodes are too many as well, the largest of them, over the detail
    /// texture if it holds an earlier frame ([`crate::refine`]). At rest,
    /// everything: at once if that fits in a frame, as [`View::draw`]
    /// does; else from the detail texture, once [`View::refine`] has drawn
    /// everything into it, and until then nothing new (`target` is left
    /// alone) unless something has changed that only a frame can show.
    ///
    /// The binding does three things with what this returns. After a
    /// `moving` frame it measures the frame's cost, from its start to when
    /// the next could start, and reports it ([`View::frame_cost`]). After a
    /// `partial` one it draws again once the view has rested, so that what
    /// stays on screen has everything. And while [`View::refining`], it
    /// calls [`View::refine`] between frames, and draws again when that
    /// says it is done.
    ///
    /// A binding that must not take a texture it will not draw on (a
    /// canvas's next frame) plans first: [`View::plan_frame`], then
    /// [`View::draw_planned`] if the plan draws.
    pub fn draw_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        now_ms: f64,
    ) -> Frame {
        let planned = self.plan_frame(device, queue, now_ms);
        self.draw_planned(device, queue, target, &planned);
        planned.frame
    }

    /// What the frame at `now_ms` is to draw ([`View::draw_frame`]), with
    /// everything it needs uploaded.
    pub fn plan_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_ms: f64,
    ) -> Planned {
        // A level of the hierarchy, while one is set, in place of the graph.
        if self.draws_level() {
            let frame = self.plan_level_frame(device, queue, now_ms);
            let how = How::Level;
            return Planned { frame, how };
        }
        // The grid once the positions rest, and the nodes in view: what a
        // frame that draws every node draws of them ([`View::prepare`]).
        self.prepare(device, queue);
        let (n, m) = self.renderer.counts();
        let total = total_units(n, m);
        let mut frame = self.budget.frame(now_ms, total);
        if frame.moving {
            self.stop_refining();
            let (nodes, edges) = split(frame.units, n, m);
            self.moving_drew = (nodes, edges);
            let under = match frame.partial {
                true => self.held_camera(),
                false => None,
            };
            let how = self.some_of(device, queue, nodes, edges, under);
            return Planned { frame, how };
        }
        let fits = self.budget.prefix(total) >= total;
        if fits || self.detail.off {
            // Everything fits in a frame: no texture is kept for it.
            self.forget_detail();
            self.renderer.release_detail();
            let how = How::Everything;
            return Planned { frame, how };
        }
        if self.refined() {
            let how = How::Refined;
            return Planned { frame, how };
        }
        frame.partial = true;
        if !self.refining_this() {
            self.stop_refining();
            self.detail.wanted = true;
        }
        if !self.detail.stale {
            // The canvas has the last moving frame, and keeps it until
            // the slices are drawn.
            let how = How::Nothing;
            return Planned { frame, how };
        }
        // Something changed that only a frame can show (a hover's ring):
        // what fits of the scene as it is now.
        frame.units = self.budget.prefix(total);
        let (nodes, edges) = split(frame.units, n, m);
        let under = self.held_camera();
        let how = self.some_of(device, queue, nodes, edges, under);
        Planned { frame, how }
    }

    /// A frame of the first `edges` edges and the `nodes` largest nodes.
    fn some_of(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nodes: usize,
        edges: usize,
        under: Option<Camera>,
    ) -> How {
        let some_nodes = (nodes < self.renderer.counts().0).then_some(nodes);
        if let Some(some) = some_nodes {
            self.gather_some(device, queue, some);
        }
        How::Some {
            edges,
            some_nodes,
            under,
        }
    }

    /// Draw what [`View::plan_frame`] planned into `target`: nothing, if
    /// the plan does not draw ([`Planned::draws`]).
    pub fn draw_planned(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        planned: &Planned,
    ) {
        let under = match planned.how {
            How::Nothing => return,
            How::Level => {
                self.draw_level(device, queue, target);
                self.detail.stale = false;
                return;
            }
            How::Everything => Under::Live {
                edges: self.renderer.counts().1,
                some_nodes: None,
            },
            How::Refined => Under::Detail,
            How::Some {
                edges,
                some_nodes,
                under: None,
            } => Under::Live { edges, some_nodes },
            How::Some {
                edges,
                some_nodes,
                under: Some(from),
            } => Under::Both {
                from,
                edges,
                some_nodes,
            },
        };
        let scale = self.radius_scale();
        self.renderer.draw_over(
            device,
            queue,
            target,
            &self.camera,
            self.draw_nodes,
            scale,
            under,
        );
        self.detail.stale = false;
    }

    /// The canvas shows the scene as it is: a frame was drawn some other
    /// way ([`View::draw`]).
    pub fn shown(&mut self) {
        self.detail.stale = false;
    }

    /// The camera or the positions changed: the next frame is a moving
    /// one, and no more slices are drawn.
    fn moved(&mut self) {
        self.budget.moved();
        self.stop_refining();
    }

    fn key(&self) -> Key {
        Key {
            radius_scale: self.radius_scale().to_bits(),
            draw_nodes: self.draw_nodes,
            counts: self.renderer.counts(),
        }
    }

    /// Whether the detail texture has everything under the highlighted
    /// edges as the camera sees it now.
    fn refined(&self) -> bool {
        (self.detail.held.as_ref()).is_some_and(|h| {
            h.progress == Progress::Done && h.camera == self.camera && h.key == self.key()
        })
    }

    /// Whether slices are being drawn for the camera as it is now.
    fn refining_this(&self) -> bool {
        (self.detail.held.as_ref()).is_some_and(|h| {
            h.progress == Progress::Refining && h.camera == self.camera && h.key == self.key()
        })
    }

    /// The camera the detail texture was drawn for, if it holds anything
    /// to draw under a frame of the scene as it is: a canvas of this size,
    /// and these nodes and edges.
    fn held_camera(&self) -> Option<Camera> {
        let held = self.detail.held.as_ref()?;
        let key = self.key();
        let same = held.cleared
            && held.camera.viewport == self.camera.viewport
            && held.key.counts == key.counts
            && held.key.draw_nodes == key.draw_nodes;
        same.then_some(held.camera)
    }

    /// Whether slices of the frame at rest remain to be drawn: call
    /// [`View::refine`] until it says there are none.
    pub fn refining(&self) -> bool {
        let held = self.detail.held.as_ref();
        self.detail.wanted || held.is_some_and(|h| h.progress == Progress::Refining)
    }

    /// Whether a frame at rest that does not fit in a frame is drawn in
    /// slices ([`View::refine`]): on unless turned off, when it is drawn
    /// at once, as [`View::draw`] draws it, however long that takes.
    pub fn set_refining(&mut self, on: bool) {
        self.detail.off = !on;
        if !on {
            self.forget_detail();
        }
    }

    /// No more slices: what the texture has stays, as a part.
    fn stop_refining(&mut self) {
        self.detail.wanted = false;
        if let Some(held) = self.detail.held.as_mut() {
            if held.progress == Progress::Refining {
                held.progress = Progress::Stopped;
                held.pending.clear();
            }
        }
    }

    /// A refinement under way begins again.
    fn restart_refining(&mut self) {
        if self.refining() {
            self.stop_refining();
            self.detail.wanted = true;
        }
    }

    /// The detail texture holds nothing of use: the scene changed.
    fn forget_detail(&mut self) {
        self.detail.wanted = false;
        self.detail.held = None;
    }

    /// The flags of the nodes in `dirty` changed: what that asks of the
    /// detail texture ([`crate::refine::change`]).
    fn flags_changed(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        dirty: &[std::ops::Range<usize>],
    ) {
        let (dims, hidden) = (self.highlight.dims_the_rest(), self.highlight.any_hidden());
        let Some(held) = self.detail.held.as_mut() else {
            return;
        };
        if (held.dims, held.hidden) != (dims, hidden) {
            return self.forget_detail();
        }
        match held.progress {
            // The slices to come would draw other flags than the ones
            // sent: begun again at the next frame at rest.
            Progress::Refining => return self.stop_refining(),
            Progress::Stopped => return,
            Progress::Done => {}
        }
        let now = self.highlight.flags();
        let was = held.missing.len();
        let mut changed = false;
        for i in dirty.iter().cloned().flatten() {
            let (Some(&before), Some(&after)) = (held.flags.get(i), now.get(i)) else {
                return self.forget_detail();
            };
            match change(before, after) {
                Change::Same | Change::Covered => changed |= held.missing.remove(&(i as u32)),
                Change::Missing => changed |= held.missing.insert(i as u32),
                Change::Redraw => return self.forget_detail(),
            }
        }
        if changed || was != held.missing.len() {
            let nodes: Vec<FrontNode> = held
                .missing
                .iter()
                .map(|&i| {
                    let i = i as usize;
                    let (radius, color) = (self.style.radius[i], self.style.color[i]);
                    FrontNode::new(self.x[i], self.y[i], radius, color, now[i])
                })
                .collect();
            self.renderer.set_patch_nodes(device, queue, &nodes);
        }
    }

    /// Draw the next slice of the frame at rest into the detail texture,
    /// if the GPU has finished the last (`now_ms` is any clock, in
    /// milliseconds: what a slice took sizes the next). Returns whether
    /// slices remain: call it again, a task later, until it says none do,
    /// and then draw a frame ([`View::draw_frame`]), which has everything.
    /// It also says none do when the view has moved or changed meanwhile:
    /// the frame drawn then begins the refinement again once the view
    /// rests.
    ///
    /// A call holds the thread for as long as it takes to send a slice or
    /// two, which is not long (1 to 6 ms at a million edges in a browser
    /// with no GPU): the GPU draws them afterwards, and is kept one slice
    /// ahead so that it does not wait for the next call.
    pub fn refine(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, now_ms: f64) -> bool {
        if std::mem::take(&mut self.detail.wanted) {
            self.begin_refining(device, now_ms);
        }
        if !self.refining_this() {
            // Not for the scene as it is (the nodes are no longer drawn):
            // the next frame at rest begins again.
            self.stop_refining();
            return false;
        }
        let m = self.renderer.counts().1;
        let flags = self.highlight.flags();
        let Detail {
            held, pace, last, ..
        } = &mut self.detail;
        let (Some(held), Some(pace)) = (held.as_mut(), pace.as_mut()) else {
            return false;
        };
        let _ = device.poll(wgpu::PollType::Poll);
        // The slices the GPU has finished since the last call: what they
        // took, together, sizes the next.
        let mut finished: Option<(Option<Kind>, usize, bool)> = None;
        while let Some(pending) = held.pending.front() {
            let timed_out = now_ms - pending.sent_ms >= SLICE_TIMEOUT_MS;
            if !pending.done.load(Ordering::Acquire) && !timed_out {
                break;
            }
            finished = Some(match finished {
                None => (pending.kind, pending.drawn, pending.whole),
                Some((kind, drawn, whole)) if kind == pending.kind => {
                    (kind, drawn + pending.drawn, whole && pending.whole)
                }
                Some(first) => first,
            });
            held.pending.pop_front();
        }
        if let Some((kind, drawn, whole)) = finished {
            let took = now_ms - held.busy_since;
            held.busy_since = now_ms;
            held.longest_ms = held.longest_ms.max(took);
            if let Some(kind) = kind {
                pace.observe(kind, drawn, whole, took);
            }
        }
        while held.pending.len() < SLICES_AHEAD {
            let (kind, slice) = if held.edges < m {
                let size = pace.slice(Kind::Edges, usize::MAX);
                let end = m.min(held.edges.saturating_add(size));
                (Some(Kind::Edges), Slice::Edges(held.edges..end))
            } else if held.nodes < held.node_total {
                let size = pace.slice(Kind::Nodes, usize::MAX);
                let end = held.node_total.min(held.nodes.saturating_add(size));
                (Some(Kind::Nodes), Slice::Nodes(held.nodes..end))
            } else if !held.cleared {
                // Nothing to draw but the background.
                (None, Slice::Nothing)
            } else if !held.pending.is_empty() {
                // The last slices are with the GPU.
                break;
            } else {
                held.progress = Progress::Done;
                held.flags = flags.to_vec();
                held.missing.clear();
                *last = Some(Refinement {
                    ms: now_ms - held.began_ms,
                    slices: held.slices,
                    longest_slice_ms: held.longest_ms,
                    slice_edges: pace.slice(Kind::Edges, usize::MAX),
                    slice_nodes: pace.slice(Kind::Nodes, usize::MAX),
                });
                self.renderer.set_patch_nodes(device, queue, &[]);
                return false;
            };
            let scale = f32::from_bits(held.key.radius_scale);
            self.renderer
                .draw_slice(device, queue, &held.camera, scale, !held.cleared, &slice);
            held.cleared = true;
            held.slices += 1;
            let (drawn, whole) = match (&slice, kind) {
                (Slice::Edges(run), Some(kind)) => {
                    held.edges = run.end;
                    (run.len(), run.len() == pace.slice(kind, usize::MAX))
                }
                (Slice::Nodes(run), Some(kind)) => {
                    held.nodes = run.end;
                    (run.len(), run.len() == pace.slice(kind, usize::MAX))
                }
                _ => (0, false),
            };
            let done = Arc::new(AtomicBool::new(false));
            let signal = done.clone();
            queue.on_submitted_work_done(move || signal.store(true, Ordering::Release));
            if held.pending.is_empty() {
                held.busy_since = now_ms;
            }
            held.pending.push_back(Pending {
                done,
                sent_ms: now_ms,
                kind,
                drawn,
                whole,
            });
        }
        true
    }

    /// A refinement of the scene as the camera sees it now: a texture the
    /// size of the canvas, and nothing in it yet.
    fn begin_refining(&mut self, device: &wgpu::Device, now_ms: f64) {
        let (w, h) = self.camera.viewport;
        self.renderer.make_detail(device, w as u32, h as u32);
        if self.detail.pace.is_none() {
            // A first slice of what a moving frame draws.
            let (n, m) = self.renderer.counts();
            let units = self.budget.prefix(total_units(n, m));
            self.detail.pace = Some(SlicePace::new(split(units, n, m).1));
        }
        let node_total = match self.draw_nodes {
            true => self.renderer.nodes_drawn() as usize,
            false => 0,
        };
        self.detail.held = Some(Held {
            camera: self.camera,
            key: self.key(),
            dims: self.highlight.dims_the_rest(),
            hidden: self.highlight.any_hidden(),
            progress: Progress::Refining,
            cleared: false,
            edges: 0,
            nodes: 0,
            node_total,
            pending: VecDeque::new(),
            busy_since: now_ms,
            began_ms: now_ms,
            slices: 0,
            longest_ms: 0.0,
            flags: Vec::new(),
            missing: BTreeSet::new(),
        });
    }

    /// Have the renderer hold at least the `count` largest nodes, as they
    /// are now: gathered again only if they changed or it holds too few
    /// (then twice as many as asked for, so that a budget that grows a
    /// little does not gather every frame).
    fn gather_some(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, count: usize) {
        let n = self.n_nodes();
        if self.some.order.len() != n {
            let radius = &self.style.radius;
            let mut order: Vec<u32> = (0..n as u32).collect();
            order.sort_unstable_by(|&a, &b| {
                radius[b as usize]
                    .total_cmp(&radius[a as usize])
                    .then(a.cmp(&b))
            });
            self.some = SomeNodes {
                order,
                fresh: false,
            };
        }
        let held = self.renderer.some_nodes() as usize;
        if self.some.fresh && held >= count.min(n) {
            return;
        }
        let gather = (2 * count).min(n);
        let flags = self.highlight.flags();
        let nodes: Vec<FrontNode> = self.some.order[..gather]
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
        self.renderer.set_some_nodes(device, queue, &nodes);
        self.some.fresh = true;
    }

    /// The view is still moving, though nothing has changed since the last
    /// frame (a layout on another thread is between two sets of
    /// positions): the next [`View::draw_frame`] is a moving one.
    pub fn keep_moving(&mut self) {
        self.moved();
    }

    /// The last moving frame ([`View::draw_frame`]) cost `ms`.
    pub fn frame_cost(&mut self, ms: f64) {
        self.budget.observe(ms);
    }

    /// Draw a frame into `target`: everything, whether the view moves or
    /// not.
    pub fn draw(&self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::TextureView) {
        if self.draws_level() {
            return self.draw_level(device, queue, target);
        }
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
