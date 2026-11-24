//! The browser binding: a [`GraphView`] on an HTML canvas, driven from
//! JavaScript.
//!
//! ```js
//! import init, { createView } from "./ursa_viz.js";
//! await init();
//! const view = await createView(canvas, nNodes, src, dst); // Uint32Arrays
//! function frame() { view.step(2); view.render(); requestAnimationFrame(frame); }
//! ```
//!
//! WebGPU is used where the browser has it, WebGL2 otherwise
//! ([`GraphView::backend`] says which). The page owns the frame loop and the
//! input events; the view only steps, draws, pans, zooms and picks.

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::highlight::{flag, BadHighlight, HighlightState};
use crate::interact::{EdgePick, Interact};
use std::sync::Arc;

use ursa_core::topology::UndirectedCsr;

use crate::model::{
    EdgeBatch, GraphDelta, GraphResult, KeyType, NodeBatch, NodeLabel, Op, Positions, Provenance,
    Source,
};
use crate::scene::{parse_color, Graph, Growth, LiveLayout, NodeStyle, Theme};
use crate::view::View;

fn js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// A graph drawn on a canvas.
#[wasm_bindgen]
pub struct GraphView {
    view: View,
    /// Kept alive with the view: Chrome's WebGPU delivers buffer maps through
    /// the instance, and rejects them once it is dropped.
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    backend: String,
    /// The selection and the hovered node.
    interact: Interact,
    /// Draw the selection's incident edges wide
    /// ([`GraphView::set_selection_edges`]).
    selection_edges: bool,
    /// The last frame left edges out ([`GraphView::render_frame`]).
    partial: bool,
    /// How many calls in a row [`GraphView::order_edges`] has waited for
    /// the GPU to take what the last one wrote.
    order_waits: u32,
}

/// A view of the graph of `n_nodes` nodes and the edges `(src[i], dst[i])`
/// (dense node indexes), on `canvas`.
#[wasm_bindgen(js_name = createView)]
pub async fn create_view(
    canvas: HtmlCanvasElement,
    n_nodes: u32,
    src: Vec<u32>,
    dst: Vec<u32>,
) -> Result<GraphView, JsValue> {
    console_error_panic_hook::set_once();
    if src.len() != dst.len() {
        return Err(js_err(format!(
            "src has {} edges but dst has {}",
            src.len(),
            dst.len()
        )));
    }
    let graph = Graph::new(n_nodes as usize, src, dst).map_err(js_err)?;
    view_on(canvas, Scene::Graph(graph)).await
}

/// A view of a graph result, on `canvas`: `bytes` is the result as UGR
/// (`ursa-viz/UGR.md`), a `Uint8Array`. Its nodes are drawn where the
/// result puts them (or where a layout would start them), sized and
/// coloured by its encodings (by degree, without), with its paths and seeds
/// highlighted and its removed nodes hidden. [`GraphView::apply_delta`]
/// then changes it in place.
///
/// The buffer stays the page's: it is not copied into this module's memory
/// whole. Each section is copied once, to where the view keeps it, with a
/// turn of the event loop between the large ones and while the result is
/// checked, as between the pieces of making the view. `bytes` must not
/// change, or be transferred, until the promise settles.
#[wasm_bindgen(js_name = createViewFromResult)]
pub async fn create_view_from_result(
    canvas: HtmlCanvasElement,
    bytes: js_sys::Uint8Array,
) -> Result<GraphView, JsValue> {
    console_error_panic_hook::set_once();
    view_on(canvas, Scene::Ugr(bytes)).await
}

/// What a new view draws.
enum Scene {
    Graph(Graph),
    /// A result as UGR, in the page's memory.
    Ugr(js_sys::Uint8Array),
}

/// How many keys of an edge order are read back at a time.
const ORDER_RUN: u32 = 1 << 15;

/// An edge order's keys in the page's (or the worker's) own memory.
struct PageKeys(js_sys::BigUint64Array);

impl ursa_core::algo::OrderKeys for PageKeys {
    fn read(&mut self, start: usize, out: &mut [u64]) {
        let run = self.0.subarray(start as u32, (start + out.len()) as u32);
        run.copy_to(out);
    }

    fn write(&mut self, start: usize, keys: &[u64]) {
        let run = self.0.subarray(start as u32, (start + keys.len()) as u32);
        run.copy_from(keys);
    }
}

/// How many calls in a row [`GraphView::order_edges`] waits for the GPU.
const ORDER_WAITS: u32 = 32;

/// A page's edge order: read in place, a run at a time.
struct PageOrder(js_sys::Uint32Array);

impl crate::order::EdgeOrder for PageOrder {
    fn len(&self) -> usize {
        self.0.length() as usize
    }

    fn read(&self, start: usize, out: &mut [u32]) {
        let run = self.0.subarray(start as u32, (start + out.len()) as u32);
        // Shorter only if the page let go of the array meanwhile: the
        // edges are then drawn in the slots of their indexes.
        if run.length() as usize == out.len() {
            run.copy_to(out);
        } else {
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = (start + i) as u32;
            }
        }
    }
}

/// A page's bytes as a [`Source`]: read in place, a range at a time.
struct PageBytes(js_sys::Uint8Array);

impl Source for PageBytes {
    fn len(&self) -> usize {
        self.0.length() as usize
    }

    fn read(&self, at: usize, out: &mut [u8]) {
        let range = self.0.subarray(at as u32, (at + out.len()) as u32);
        // Shorter only if the page let go of the buffer meanwhile (it was
        // transferred): what is read then fails the result's checks, or
        // draws what the zeros say; it is not read past its end.
        if range.length() as usize == out.len() {
            range.copy_to(out);
        } else {
            out.fill(0);
        }
    }
}

/// The most selected nodes whose edges are drawn wide: a larger selection
/// (a group of the aggregated view, #498) is its nodes' rings alone.
const SELECTION_EDGES_MOST_NODES: usize = 10_000;

/// A graph of this many nodes and edges is drawn a level at a time while
/// it is laid out (the shell's layout worker says which; this is its
/// threshold): its view warms the level pipelines as it is made.
const LEVELS_FROM: usize = 200_000;

async fn view_on(canvas: HtmlCanvasElement, scene: Scene) -> Result<GraphView, JsValue> {
    let (width, height) = (canvas.width().max(1), canvas.height().max(1));
    let instance = wgpu::util::new_instance_with_webgpu_detection(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
    .await;
    // `create_surface(SurfaceTarget::Canvas)` passes no display handle, and
    // WebGL2 needs one: name the web's, which carries nothing.
    let surface = {
        use wgpu::rwh::{
            RawDisplayHandle, RawWindowHandle, WebCanvasWindowHandle, WebDisplayHandle,
        };
        let value: &JsValue = canvas.as_ref();
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Web(WebDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::WebCanvas(WebCanvasWindowHandle::new(
                std::ptr::NonNull::from(value).cast(),
            )),
        };
        // SAFETY: the handle points at `canvas`, which is alive for this call,
        // and wgpu keeps its own reference to the canvas for the surface.
        unsafe { instance.create_surface_unsafe(target) }.map_err(js_err)?
    };
    // Making the canvas's context is a piece of its own (20 ms of software
    // WebGL2), and so is asking for the adapter and the device.
    crate::pace::yield_now().await;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        })
        .await
        .map_err(js_err)?;
    let info = adapter.get_info();
    let backend = format!("{:?}", info.backend);
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("ursa-viz"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                .using_resolution(adapter.limits()),
            ..Default::default()
        })
        .await
        .map_err(js_err)?;
    // A GPU error nothing waits on (validation, out of memory) goes to the
    // console, where it can be seen, instead of nowhere.
    device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
        web_sys::console::error_1(&format!("ursa-viz: {e}").into());
    }));
    let mut config = surface
        .get_default_config(&adapter, width, height)
        .ok_or_else(|| js_err("the canvas cannot be drawn on by this adapter"))?;
    // Colours are sRGB values already: draw to the plain format where the
    // canvas offers one, so they are not gamma-encoded twice.
    let plain = config.format.remove_srgb_suffix();
    if surface.get_capabilities(&adapter).formats.contains(&plain) {
        config.format = plain;
    }
    surface.configure(&device, &config);
    // In pieces, with a turn of the event loop between them: the page's
    // thread is never held for long.
    let view = match scene {
        Scene::Graph(graph) => {
            View::new_paced(&device, &queue, config.format, graph, width, height).await
        }
        Scene::Ugr(bytes) => {
            let bytes = PageBytes(bytes);
            View::from_ugr_paced(&device, &queue, config.format, &bytes, width, height)
                .await
                .map_err(js_err)?
        }
    };
    let mut view = view;
    // A graph this large is first seen as a level of its layout's
    // hierarchy (#497): what draws one is made now, not in that frame.
    if view.n_nodes() + view.n_edges() >= LEVELS_FROM {
        // In tasks of its own, a piece at a time (#520): compiling is not
        // quick everywhere.
        let mut pace = crate::pace::Pace::yielding();
        view.warm_levels_paced(&device, &queue, config.format, &mut pace)
            .await;
    }
    // What the caller does with the view is a task of its own, not the
    // tail of the last piece here (the level's first draw, or the last
    // run of edges): on the shell's page that is 12 to 27 ms more (#520).
    crate::pace::yield_now().await;
    Ok(GraphView {
        view,
        _instance: instance,
        surface,
        config,
        device,
        queue,
        backend,
        interact: Interact::new(),
        selection_edges: false,
        partial: false,
        order_waits: 0,
    })
}

#[wasm_bindgen]
impl GraphView {
    /// `"BrowserWebGpu"` or `"Gl"` (WebGL2).
    #[wasm_bindgen(getter)]
    pub fn backend(&self) -> String {
        self.backend.clone()
    }

    /// Whether the view has built the graph's adjacency: only
    /// [`GraphView::step`], a layout on the view's own thread, makes it.
    #[wasm_bindgen(getter, js_name = hasTopology)]
    pub fn has_topology(&self) -> bool {
        self.view.has_adjacency()
    }

    #[wasm_bindgen(getter, js_name = nodeCount)]
    pub fn node_count(&self) -> u32 {
        self.view.n_nodes() as u32
    }

    #[wasm_bindgen(getter, js_name = edgeCount)]
    pub fn edge_count(&self) -> u32 {
        self.view.n_edges() as u32
    }

    /// Layout iterations run so far.
    #[wasm_bindgen(getter)]
    pub fn iterations(&self) -> u32 {
        self.view.iterations()
    }

    /// Run `iterations` more layout iterations.
    pub fn step(&mut self, iterations: u32) {
        self.view.step(&self.device, &self.queue, iterations);
    }

    /// Draw a frame: everything, whether the view moves or not.
    pub fn render(&mut self) -> Result<(), JsValue> {
        self.present(None).map(|_| ())
    }

    /// Whether slices of the frame at rest remain to be drawn: the frame
    /// with everything does not fit in a frame, so
    /// [`GraphView::render_frame`] left the canvas as it was, and
    /// [`GraphView::refine`] draws it a slice a call.
    #[wasm_bindgen(getter)]
    pub fn refining(&self) -> bool {
        self.view.refining()
    }

    /// Draw the next slice of the frame at rest, offscreen, if the GPU has
    /// finished the last; `now` is `performance.now()`. Returns whether
    /// slices remain. Call it a task at a time while it returns true, and
    /// then draw a frame ([`GraphView::render_frame`]): the frame with
    /// everything if the slices are done, and if the view moved or changed
    /// meanwhile the frame that shows it, which begins them again once the
    /// view rests.
    ///
    /// ```js
    /// const refine = () => {
    ///   if (view.refine(performance.now())) setTimeout(refine);
    ///   else requestAnimationFrame(draw);
    /// };
    /// ```
    pub fn refine(&mut self, now: f64) -> bool {
        self.view.refine(&self.device, &self.queue, now)
    }

    /// Whether a frame at rest that does not fit in a frame is drawn in
    /// slices: on unless turned off, when [`GraphView::render_frame`]
    /// draws it at once, as [`GraphView::render`] does.
    #[wasm_bindgen(js_name = setRefining)]
    pub fn set_refining(&mut self, on: bool) {
        self.view.set_refining(on);
    }

    /// [`GraphView::snapshot`], of the frame a refined view at rest shows:
    /// the texture the slices were drawn into, under the highlighted edges
    /// and the nodes in front. `undefined` unless the view is refined
    /// (`frameInfo().refined`). Call no other method until it resolves.
    #[wasm_bindgen(js_name = snapshotRefined)]
    pub async fn snapshot_refined(&self) -> Result<Option<Vec<u8>>, JsValue> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pixels = self
            .view
            .snapshot_refined(&self.device, &self.queue, self.config.format)
            .await;
        if let Some(e) = scope.pop().await {
            return Err(js_err(format!("snapshot: {e}")));
        }
        pixels.transpose().map_err(js_err)
    }

    /// Draw a frame within the frame budget: while the view moves (the
    /// camera or the positions changed since the last frame, or less than
    /// [`GraphView::frame_rest_ms`] before `now`, the animation frame's
    /// time),
    /// only as many edges as the last frames' costs say fit in a frame,
    /// the most important first ([`GraphView::set_edge_order`]), drawn
    /// deeper to make up for the rest, and, where the nodes are too many
    /// as well, the largest of them; at rest, everything, the same pixels
    /// as [`GraphView::render`].
    ///
    /// Everything at rest is drawn at once if that fits in a frame. If it
    /// does not, this frame leaves the canvas as it is (or, if something
    /// changed that it must show, draws what fits), and
    /// [`GraphView::refining`] is true: draw the rest a slice a task with
    /// [`GraphView::refine`], and then draw again. That frame, and the
    /// frames at rest after it, are a texture with everything in it; the
    /// frames of the next pan or zoom draw it under their own edges and
    /// nodes, moved and scaled as the camera is.
    ///
    /// Returns whether the view is moving. If it is, measure what the
    /// frame cost, from this animation frame's time to the next's (whole
    /// refreshes of the display, whenever in its frame each callback ran),
    /// and say ([`GraphView::frame_cost`]): the budget follows it. And if
    /// [`GraphView::frame_partial`] is then true, something was left out:
    /// draw again once nothing has moved for `frameRestMs`.
    ///
    /// ```js
    /// function draw(time) {  // an animation frame's
    ///   if (view.renderFrame(time)) {
    ///     requestAnimationFrame((next) => view.frameCost(next - time));
    ///     clearTimeout(rest);
    ///     if (view.framePartial) rest = setTimeout(() => requestAnimationFrame(draw), view.frameRestMs);
    ///   } else if (view.refining) {
    ///     refine();  // above
    ///   }
    /// }
    /// ```
    #[wasm_bindgen(js_name = renderFrame)]
    pub fn render_frame(&mut self, now: f64) -> Result<bool, JsValue> {
        self.present(Some(now))
    }

    /// Draw and present a frame: budgeted at `now`, or everything.
    /// Returns whether the view is moving.
    fn present(&mut self, now: Option<f64>) -> Result<bool, JsValue> {
        let planned = now.map(|now| self.view.plan_frame(&self.device, &self.queue, now));
        if let Some(planned) = planned.filter(|p| !p.draws()) {
            // The canvas keeps its frame while the slices are drawn.
            self.partial = planned.frame.partial;
            return Ok(planned.frame.moving);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(false);
            }
            other => return Err(js_err(format!("no frame to draw on: {other:?}"))),
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let moving = match planned {
            Some(planned) => {
                self.view
                    .draw_planned(&self.device, &self.queue, &target, &planned);
                self.partial = planned.frame.partial;
                planned.frame.moving
            }
            None => {
                self.view.prepare(&self.device, &self.queue);
                self.view.draw(&self.device, &self.queue, &target);
                self.view.shown();
                self.partial = false;
                false
            }
        };
        frame.present();
        Ok(moving)
    }

    /// The view is still moving, though nothing has changed since the last
    /// frame: a layout is running, and its next positions are on their
    /// way. The next [`GraphView::render_frame`] draws within the budget.
    #[wasm_bindgen(js_name = keepMoving)]
    pub fn keep_moving(&mut self) {
        self.view.keep_moving();
    }

    /// Whether the canvas lacks anything after the last frame
    /// ([`GraphView::render_frame`]): a moving frame left edges or nodes
    /// out, or the frame at rest is still being drawn in slices.
    #[wasm_bindgen(getter, js_name = framePartial)]
    pub fn frame_partial(&self) -> bool {
        self.partial
    }

    /// How long after the last movement the view is at rest, in
    /// milliseconds: a frame drawn then has every edge.
    #[wasm_bindgen(getter, js_name = frameRestMs)]
    pub fn frame_rest_ms(&self) -> f64 {
        crate::budget::REST_MS
    }

    /// The last moving frame ([`GraphView::render_frame`]) cost `ms`: the
    /// time from its start to when the next frame could start (the next
    /// animation frame). With no GPU that is the time the browser took to
    /// rasterise it; with one it is one refresh of the display, or more if
    /// the frame was late.
    #[wasm_bindgen(js_name = frameCost)]
    pub fn frame_cost(&mut self, ms: f64) {
        self.view.frame_cost(ms);
    }

    /// What the frame budget is doing:
    ///
    /// ```js
    /// { k,        // the edges the last moving frame drew (0 before any)
    ///   nodes,    // the nodes it drew: the largest
    ///   edges,    // the edges a frame at rest draws
    ///   ordered,  // how many of them are in importance order
    ///   p95,      // of the last 120 moving frames' costs, ms; null before any
    ///   frames,   // how many moving frames' costs were measured
    ///   limit,    // the cost a moving frame is held to, ms
    ///   refining, // whether slices of the frame at rest remain
    ///   refined,  // whether the frame at rest is a texture with everything
    ///   refinement } // the last one finished: { ms, slices, longestSliceMs,
    ///                // sliceEdges, sliceNodes }; null before any
    /// ```
    #[wasm_bindgen(js_name = frameInfo)]
    pub fn frame_info(&self) -> Result<JsValue, JsValue> {
        let json = serde_json::to_string(&self.view.frame_info()).map_err(js_err)?;
        js_sys::JSON::parse(&json)
    }

    /// Draw the edges in `order`, most important first, as
    /// [`Layout::edge_order`] gives it: `order[i]` is the edge to draw
    /// `i`-th, a permutation of the first `order.length` edges. Edges added
    /// since stay after them. The frames that draw every edge do not
    /// change; the ones that draw a prefix ([`GraphView::render_frame`])
    /// draw the backbone. Nothing is written until
    /// [`GraphView::order_edges`].
    ///
    /// `order` is a `Uint32Array`, and stays the page's until the order is
    /// whole: the view reads it a run at a time and holds no copy of it
    /// (#501). It must not change, or be transferred, until
    /// [`GraphView::order_edges`] returns false.
    #[wasm_bindgen(js_name = setEdgeOrder)]
    pub fn set_edge_order(&mut self, order: js_sys::Uint32Array) -> Result<(), JsValue> {
        self.order_waits = 0;
        self.view
            .set_edge_order_from(&self.device, Box::new(PageOrder(order)))
            .map_err(js_err)
    }

    /// Write up to `max_edges` (65,536 unless given) more edges of the
    /// order being set to the GPU, eight bytes each, and return whether
    /// there is more to write. Call it a task at a time until it returns
    /// false: the frames draw the old order until the new one is whole.
    ///
    /// What is written is staged in this module's memory until the GPU has
    /// taken it (WebGL2). With `wait` true, a call made while the GPU
    /// still holds the last one's writes nothing and returns true, up to
    /// 32 times in a row: the order then costs the view a run of staging
    /// and not all of it. A page that calls this a task at a time passes
    /// it.
    #[wasm_bindgen(js_name = orderEdges)]
    pub fn order_edges(&mut self, max_edges: Option<u32>, wait: Option<bool>) -> bool {
        if !self.view.is_ordering() {
            return false;
        }
        let taken = !wait.unwrap_or(false)
            || self
                .device
                .poll(wgpu::PollType::Poll)
                .map_or(true, |status| status.is_queue_empty());
        if !taken && self.order_waits < ORDER_WAITS {
            self.order_waits += 1;
            return true;
        }
        self.order_waits = 0;
        let max = max_edges.unwrap_or(1 << 16) as usize;
        self.view.order_edges(&self.device, &self.queue, max)
    }

    /// The canvas is now `width` x `height` pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.view.resize(width, height);
    }

    /// A drag of `(dx, dy)` pixels.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.view.pan(dx, dy);
    }

    /// Zoom by `factor` around the pixel `(x, y)`.
    pub fn zoom(&mut self, factor: f32, x: f32, y: f32) {
        self.view.zoom(factor, x, y);
    }

    /// Fit the layout in view, and follow it as it moves.
    pub fn fit(&mut self) {
        self.view.fit();
    }

    /// Leave `pixels` free at each side of the canvas when the camera
    /// fits: for discs drawn larger around a node than a node is
    /// (`GROUP BY`'s groups, #499). 0 is the fit as it was. With `first`,
    /// a fit is of the first `first` nodes alone: the groups, whose
    /// members are drawn inside their discs.
    #[wasm_bindgen(js_name = setFitPadding)]
    pub fn set_fit_padding(&mut self, pixels: f32, first: Option<u32>) {
        self.view.set_fit_padding(pixels, first.map(|k| k as usize));
    }

    /// A frame drawn offscreen: RGBA pixels, canvas-sized, row by row from
    /// the top (`new ImageData(new Uint8ClampedArray(px), width)`). Call no
    /// other method until it resolves.
    pub async fn snapshot(&self) -> Result<Vec<u8>, JsValue> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pixels = self
            .view
            .snapshot(&self.device, &self.queue, self.config.format)
            .await;
        if let Some(e) = scope.pop().await {
            return Err(js_err(format!("snapshot: {e}")));
        }
        pixels.map_err(js_err)
    }

    /// The node under the pixel `(x, y)`, or `undefined`. With the
    /// positions at rest it reads only the nodes near the pixel
    /// ([`GraphView::grid_ready`]).
    pub fn pick(&mut self, x: f32, y: f32) -> Option<u32> {
        self.view.pick(x, y)
    }

    /// [`GraphView::pick`], testing every node: the same answer, for a
    /// test to compare.
    #[wasm_bindgen(js_name = pickLinear)]
    pub fn pick_linear(&self, x: f32, y: f32) -> Option<u32> {
        self.view.pick_linear(x, y)
    }

    /// Whether the grid over the nodes has every node where it is now. The
    /// view builds it once the positions rest (three frames or picks with
    /// none changed); until then, and while a layout runs, a pick tests
    /// every node, and every node is drawn.
    #[wasm_bindgen(getter, js_name = gridReady)]
    pub fn grid_ready(&self) -> bool {
        self.view.grid_ready()
    }

    /// Build the grid over the nodes now, without waiting for the
    /// positions to rest: for timing it.
    #[wasm_bindgen(js_name = rebuildGrid)]
    pub fn rebuild_grid(&mut self) {
        self.view.rebuild_grid();
    }

    /// Whether a zoomed-in frame may draw the nodes in view alone: on
    /// unless turned off. The frame is the same either way.
    #[wasm_bindgen(js_name = setCulling)]
    pub fn set_culling(&mut self, on: bool) {
        self.view.set_culling(on);
    }

    /// How many node instances the last frame's pass over the nodes drew:
    /// every node, or, zoomed in with the positions at rest, the nodes in
    /// view.
    #[wasm_bindgen(getter, js_name = nodesDrawn)]
    pub fn nodes_drawn(&self) -> u32 {
        self.view.nodes_drawn() as u32
    }

    /// Node radii from a numeric column, one value per node.
    #[wasm_bindgen(js_name = sizeBy)]
    pub fn size_by(&mut self, values: Vec<f32>, min_px: f32, max_px: f32) -> Result<(), JsValue> {
        self.check_len(values.len())?;
        let mut style = self.view.style().clone();
        style.radius = NodeStyle::sizes_from(&values, min_px, max_px);
        self.view.set_style(&self.device, &self.queue, style);
        Ok(())
    }

    /// Node colours from a categorical column, one category per node:
    /// category `c` is the theme's `palette[c % palette.length]`. Kept, so
    /// [`GraphView::set_theme`] recolours the nodes.
    #[wasm_bindgen(js_name = colorBy)]
    pub fn color_by(&mut self, categories: Vec<u32>) -> Result<(), JsValue> {
        self.check_len(categories.len())?;
        self.view
            .color_by(&self.device, &self.queue, Some(categories));
        Ok(())
    }

    /// The colours to draw with, as CSS hex colours (`#rrggbb`, `#rgb`,
    /// with alpha or not): `{background, edge, node, palette, highlight,
    /// selection, pin}`, each optional, a missing one kept. `edge` is the
    /// edges' ink (their opacity is [`GraphView::set_edge_alpha`] times its
    /// alpha), `node` the nodes' colour when they are not coloured by
    /// category, and `palette` the categories' colours (an array).
    /// `highlight` colours highlighted edges, halos and the rings of a
    /// route's ends; `selection` the rings of selected and hovered nodes;
    /// `pin` the rings of pinned ones. Takes effect on the next
    /// [`GraphView::render`]; the default is ink on white.
    #[wasm_bindgen(js_name = setTheme)]
    pub fn set_theme(&mut self, theme: JsValue) -> Result<(), JsValue> {
        let mut next = self.view.theme().clone();
        let field = |name: &str| js_sys::Reflect::get(&theme, &JsValue::from_str(name));
        let color = |name: &str, value: JsValue| -> Result<u32, JsValue> {
            let text = value
                .as_string()
                .ok_or_else(|| js_err(format!("setTheme: {name} must be a CSS hex colour")))?;
            parse_color(&text).ok_or_else(|| {
                js_err(format!(
                    "setTheme: {name} must be a CSS hex colour, as #0b0b0a; got '{text}'"
                ))
            })
        };
        for (name, slot) in [
            ("background", &mut next.background),
            ("edge", &mut next.edge),
            ("node", &mut next.node),
            ("highlight", &mut next.highlight),
            ("selection", &mut next.selection),
            ("pin", &mut next.pin),
        ] {
            let value = field(name)?;
            if !value.is_undefined() && !value.is_null() {
                *slot = color(name, value)?;
            }
        }
        let palette = field("palette")?;
        if !palette.is_undefined() && !palette.is_null() {
            if !js_sys::Array::is_array(&palette) {
                return Err(js_err(
                    "setTheme: palette must be an array of CSS hex colours",
                ));
            }
            next.palette = js_sys::Array::from(&palette)
                .iter()
                .map(|c| color("palette", c))
                .collect::<Result<_, _>>()?;
        }
        self.view.set_theme(&self.device, &self.queue, next);
        Ok(())
    }

    /// The built-in themes' colours: `"light"` (ink on white, the default)
    /// or `"dark"` (light on near-black), as [`GraphView::set_theme`] takes
    /// them.
    #[wasm_bindgen(js_name = setThemeNamed)]
    pub fn set_theme_named(&mut self, name: &str) -> Result<(), JsValue> {
        let theme = match name {
            "light" => Theme::light(),
            "dark" => Theme::dark(),
            other => {
                return Err(js_err(format!(
                    "no theme named '{other}'; use light or dark"
                )))
            }
        };
        self.view.set_theme(&self.device, &self.queue, theme);
        Ok(())
    }

    /// Draw nodes as well as edges (on by default).
    #[wasm_bindgen(setter, js_name = drawNodes)]
    pub fn set_draw_nodes(&mut self, on: bool) {
        self.view.set_draw_nodes(on);
    }

    /// Edge opacity, 0 to 1.
    #[wasm_bindgen(setter, js_name = edgeAlpha)]
    pub fn set_edge_alpha(&mut self, alpha: f32) {
        self.view.renderer_mut().edge_alpha = alpha.clamp(0.0, 1.0);
    }

    /// Every node's flags, one byte each ([`NodeFlag`]'s bits, or'd):
    /// selected, hovered and pinned draw a ring; highlighted a halo; a seed
    /// (or a route's end) a ring in the highlight colour; dimmed draws the
    /// node at 35%; hidden draws neither the node nor its edges. While any
    /// node is highlighted or a seed, or any edge is highlighted, the edges
    /// and every node that is not selected, hovered, highlighted or a seed
    /// draw at 35%.
    ///
    /// Only the flags that changed are uploaded (four bytes a node:
    /// [`GraphView::flag_bytes_uploaded`]), never the edges or the styles.
    #[wasm_bindgen(js_name = setFlags)]
    pub fn set_flags(&mut self, flags: &[u8]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_flags(flags))
    }

    /// Every node's flags, as [`GraphView::set_flags`] takes them.
    pub fn flags(&self) -> Vec<u8> {
        self.view.highlight().flags().to_vec()
    }

    /// The edges to draw highlighted: `src, dst` node indexes per edge, wide
    /// (3 px) in the theme's highlight colour, over the others. They need
    /// not be edges of the graph. Each uploads its two ends' positions (16
    /// bytes), again when the positions change; the graph's edges are never
    /// uploaded again. An empty array clears them.
    #[wasm_bindgen(js_name = setHighlightEdges)]
    pub fn set_highlight_edges(&mut self, pairs: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_edges(pairs))
    }

    /// Highlight paths and seeds, replacing the highlight there was: the
    /// paths' nodes get a halo, the steps between their consecutive nodes
    /// are drawn wide, each path's two ends and each seed carry a ring, and
    /// everything else is dimmed. The arguments are UGR's sections
    /// (`highlights.paths.offsets`, `highlights.paths.nodes`,
    /// `highlights.seeds`): path `p` is `nodes[offsets[p]..offsets[p + 1]]`.
    /// `seeds` may be left out. Selection, hover, pins and hidden nodes are
    /// kept.
    #[wasm_bindgen(js_name = setHighlights)]
    pub fn set_highlights(
        &mut self,
        offsets: &[u32],
        nodes: &[u32],
        seeds: Option<Vec<u32>>,
    ) -> Result<(), JsValue> {
        self.highlight(|h| h.set_paths(offsets, nodes, seeds.as_deref().unwrap_or(&[])))
    }

    /// No path, seed or highlighted edge: nothing is dimmed. Selection,
    /// hover, pins and hidden nodes are kept.
    #[wasm_bindgen(js_name = clearHighlights)]
    pub fn clear_highlights(&mut self) -> Result<(), JsValue> {
        self.highlight(|h| {
            h.clear_highlight();
            Ok(())
        })
    }

    /// The selected nodes: exactly these, each with a ring in the selection
    /// colour (and, with [`GraphView::set_selection_edges`], their edges
    /// drawn wide). An empty array selects none.
    #[wasm_bindgen(js_name = setSelection)]
    pub fn set_selection(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.check_nodes(nodes)?;
        self.interact.selection_mut().set(nodes);
        self.draw_selection()
    }

    /// A click, for the selection: on `node` (as [`GraphView::pick`] gave
    /// it) or on empty space (`undefined`), with Shift held (`extend`) or
    /// not. A plain click on a node selects it alone, and on empty space
    /// clears the selection; with `extend`, a click on a node adds it (or
    /// takes it out, if it was selected) and a click on empty space changes
    /// nothing. Returns whether the selection changed, and so the frame.
    ///
    /// A selected edge ([`GraphView::select_edge`]) is deselected by any
    /// click but one on empty space with `extend`.
    pub fn select(&mut self, node: Option<u32>, extend: bool) -> Result<bool, JsValue> {
        self.check_nodes(node.as_slice())?;
        let changed = self.interact.click(node, extend);
        if changed {
            self.draw_selection()?;
        }
        Ok(changed)
    }

    /// The selected nodes, in the order they were selected.
    pub fn selection(&self) -> Vec<u32> {
        self.interact.selection().nodes().to_vec()
    }

    /// Select nothing: no node and no edge. Returns whether anything was
    /// selected.
    #[wasm_bindgen(js_name = clearSelection)]
    pub fn clear_selection(&mut self) -> Result<bool, JsValue> {
        let changed = self.interact.clear();
        if changed {
            self.draw_selection()?;
        }
        Ok(changed)
    }

    /// The edge drawn nearest the pixel `(x, y)`, within `tolerance` pixels
    /// of it (4 CSS pixels, as a rule), or `undefined`: its index. For a
    /// click with no node under it ([`GraphView::pick`] first). An edge of
    /// a hidden node is not there to pick, nor is a removed edge. It reads
    /// the edges near the pixel when the positions are at rest, and every
    /// edge otherwise (`interact::nearest_edge`).
    #[wasm_bindgen(js_name = pickEdge)]
    pub fn pick_edge(&mut self, x: f32, y: f32, tolerance: f32) -> Option<u32> {
        self.view.pick_edge(x, y, tolerance)
    }

    /// [`GraphView::pick_edge`], testing every edge: the same answer, for
    /// a test to compare.
    #[wasm_bindgen(js_name = pickEdgeLinear)]
    pub fn pick_edge_linear(&self, x: f32, y: f32, tolerance: f32) -> Option<u32> {
        self.view.pick_edge_linear(x, y, tolerance)
    }

    /// Select the edge `edge` alone (as [`GraphView::pick_edge`] gave it):
    /// it is drawn wide, and no node is selected. `undefined` deselects the
    /// edge and leaves the nodes. Returns whether anything changed, and so
    /// the frame.
    #[wasm_bindgen(js_name = selectEdge)]
    pub fn select_edge(&mut self, edge: Option<u32>) -> Result<bool, JsValue> {
        let (src, dst) = self.view.edges();
        let pick = match edge {
            None => None,
            Some(e) if (e as usize) < src.len() => Some(EdgePick {
                edge: e,
                src: src[e as usize],
                dst: dst[e as usize],
            }),
            Some(e) => {
                return Err(js_err(format!(
                    "edge {e} is not one of the graph's {} edges",
                    src.len()
                )))
            }
        };
        let changed = self.interact.select_edge(pick);
        if changed {
            self.draw_selection()?;
        }
        Ok(changed)
    }

    /// The selected edge, as `[edge, src, dst]` (its index and the nodes it
    /// joins), or `undefined`.
    #[wasm_bindgen(js_name = selectedEdge)]
    pub fn selected_edge(&self) -> Option<Vec<u32>> {
        self.interact.edge().map(|e| vec![e.edge, e.src, e.dst])
    }

    /// Whether the selected nodes' incident edges are drawn wide, in the
    /// highlight colour, over the others (off at first). They dim nothing.
    /// Each selection change then reads every edge once, and uploads 16
    /// bytes for each edge at a selected node.
    #[wasm_bindgen(setter, js_name = selectionEdges)]
    pub fn set_selection_edges(&mut self, on: bool) -> Result<(), JsValue> {
        if self.selection_edges != on {
            self.selection_edges = on;
            self.draw_selection()?;
        }
        Ok(())
    }

    /// The node under the pointer, with a thin ring, or none (`undefined`).
    #[wasm_bindgen(js_name = setHovered)]
    pub fn set_hovered(&mut self, node: Option<u32>) -> Result<(), JsValue> {
        self.check_nodes(node.as_slice())?;
        if self.interact.hover(node) {
            self.highlight(|h| h.set_only(flag::HOVERED, node.as_slice()))?;
        }
        Ok(())
    }

    /// The pointer is at the canvas pixel `(x, y)`: the node under it
    /// ([`GraphView::pick`]) becomes the hovered one. Returns it. Nothing
    /// is uploaded while the pointer stays on one node, or on none.
    pub fn hover(&mut self, x: f32, y: f32) -> Result<Option<u32>, JsValue> {
        let node = self.view.pick(x, y);
        self.set_hovered(node)?;
        Ok(node)
    }

    /// The hovered node, if any.
    #[wasm_bindgen(getter)]
    pub fn hovered(&self) -> Option<u32> {
        self.interact.hovered()
    }

    fn check_nodes(&self, nodes: &[u32]) -> Result<(), JsValue> {
        let n = self.view.n_nodes();
        match nodes.iter().find(|&&v| v as usize >= n) {
            Some(&node) => Err(js_err(BadHighlight::Node { node, nodes: n })),
            None => Ok(()),
        }
    }

    /// Draw the selection as it is now: its nodes' rings, and their
    /// incident edges if asked for; or the selected edge.
    fn draw_selection(&mut self) -> Result<(), JsValue> {
        let selection = self.interact.selection();
        let nodes = selection.nodes().to_vec();
        let edges = match self.interact.edge() {
            // A selected edge is drawn wide itself; no node is selected.
            Some(e) => vec![e.src, e.dst],
            // Not under a cut, which draws none of the graph's edges, nor
            // for more nodes than a search selects (a group of a cut can
            // be half the graph): rings alone.
            None if self.selection_edges
                && !self.view.draws_cut()
                && nodes.len() <= SELECTION_EDGES_MOST_NODES =>
            {
                let (src, dst) = self.view.edges();
                selection.incident_edges(src, dst)
            }
            None => Vec::new(),
        };
        self.highlight(|h| {
            h.set_only(flag::SELECTED, &nodes)?;
            h.set_selection_edges(&edges)
        })
    }

    /// The pinned nodes: exactly these, each with a small ring in the pin
    /// colour. The view only draws the pin; holding the node in place is
    /// the layout's ([`Layout::grow`]'s `pinned`).
    #[wasm_bindgen(js_name = setPinned)]
    pub fn set_pinned(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::PINNED, nodes))
    }

    /// The hidden nodes: exactly these. A hidden node is not drawn, nor
    /// are its edges, and it cannot be picked; it keeps its index. It
    /// leaves the selection.
    #[wasm_bindgen(js_name = setHidden)]
    pub fn set_hidden(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::HIDDEN, nodes))?;
        self.keep_interaction()
    }

    /// Bytes of flags written to the GPU so far: 4 for each node whose
    /// flags changed.
    #[wasm_bindgen(getter, js_name = flagBytesUploaded)]
    pub fn flag_bytes_uploaded(&self) -> f64 {
        self.view.renderer().flag_bytes_uploaded() as f64
    }

    /// Bytes of the highlight layer's own instances written to the GPU so
    /// far: 16 for each highlighted edge and 20 for each node drawn in
    /// front (selected, hovered, highlighted or a seed), each time they or
    /// the positions change.
    #[wasm_bindgen(getter, js_name = highlightBytesUploaded)]
    pub fn highlight_bytes_uploaded(&self) -> f64 {
        let renderer = self.view.renderer();
        (renderer.highlight_bytes_uploaded() + renderer.front_bytes_uploaded()) as f64
    }

    /// Where node `node` is drawn: `[x, y]` in canvas pixels from the top
    /// left.
    #[wasm_bindgen(js_name = nodePixel)]
    pub fn node_pixel(&self, node: u32) -> Result<Vec<f32>, JsValue> {
        let n = self.view.n_nodes();
        if node as usize >= n {
            return Err(js_err(BadHighlight::Node { node, nodes: n }));
        }
        let (x, y) = self.view.node_pixel(node as usize);
        Ok(vec![x, y])
    }

    fn highlight(
        &mut self,
        change: impl FnOnce(&mut HighlightState) -> Result<(), BadHighlight>,
    ) -> Result<(), JsValue> {
        self.view
            .set_highlight(&self.device, &self.queue, change)
            .map_err(js_err)
    }

    /// Apply a delta to the scene in place: `bytes` is a delta as UGR, for
    /// a view made by [`create_view_from_result`]. New nodes and edges are
    /// written after the ones there are, removed nodes and edges are hidden
    /// in their slots, and only the styles that changed are written, so it
    /// costs what the delta holds, not what the graph does. No node's or
    /// edge's index changes.
    ///
    /// **Undo** is the inverse delta ([`invert_delta`]) applied the same
    /// way, with `positions` (`x` then `y`, one per node after the delta,
    /// as [`GraphView::positions`] gave them before the step) to put every
    /// node back where it was. A delta and its inverse leave the frame as
    /// it was, bit for bit.
    ///
    /// A new node the delta gives no position starts beside a node a new
    /// edge joins it to, until the layout places it ([`Layout::grow`], then
    /// [`GraphView::set_positions`]).
    ///
    /// If the delta does not fit the result, this throws and nothing
    /// changes. Returns what it did ([`DeltaStats`]).
    #[wasm_bindgen(js_name = applyDelta)]
    pub fn apply_delta(
        &mut self,
        bytes: &[u8],
        positions: Option<Vec<f32>>,
    ) -> Result<DeltaStats, JsValue> {
        let delta = GraphDelta::from_bytes(bytes).map_err(js_err)?;
        let xy = positions.as_deref().map(|xy| xy.split_at(xy.len() / 2));
        let applied = self
            .view
            .apply_at(&self.device, &self.queue, &delta, xy)
            .map_err(js_err)?;
        self.keep_interaction()?;
        let (node_capacity, edge_capacity) = self.view.renderer().capacity();
        Ok(DeltaStats {
            nodes: applied.changes.nodes.1 as u32,
            edges: applied.changes.edges.1 as u32,
            bytes_uploaded: applied.bytes_uploaded as f64,
            grew: applied.grew,
            node_capacity: node_capacity as u32,
            edge_capacity: edge_capacity as u32,
        })
    }

    /// After the graph changed: the selection and the hover drop the nodes
    /// that are gone or hidden (and a selected edge that is gone, or at such
    /// a node), and a selection's edges are found again.
    /// Nothing is read while nothing is selected or hovered.
    fn keep_interaction(&mut self) -> Result<(), JsValue> {
        let picked = self.interact.edge();
        if self.interact.selection().is_empty()
            && self.interact.hovered().is_none()
            && picked.is_none()
        {
            return Ok(());
        }
        let flags = self.view.highlight().flags();
        let shown = |v: u32| flags.get(v as usize).is_some_and(|f| f & flag::HIDDEN == 0);
        let (mut selection, hover) = self.interact.retain(shown);
        // The selected edge may be gone itself: popped, or removed.
        if let Some(e) = self.interact.edge() {
            let (src, dst) = self.view.edges();
            let at = e.edge as usize;
            let there = at < src.len()
                && (src[at], dst[at]) == (e.src, e.dst)
                && self.view.result().is_none_or(|r| r.edge_is_shown(at));
            if !there {
                self.interact.select_edge(None);
                selection = true;
            }
        }
        if hover {
            self.highlight(|h| h.set_only(flag::HOVERED, &[]))?;
        }
        if selection || self.selection_edges && !self.interact.selection().is_empty() {
            self.draw_selection()?;
        }
        Ok(())
    }

    /// What is shown now, for a status line and a legend, for a view made
    /// from a result (`undefined` otherwise):
    ///
    /// ```js
    /// { nodes, edges,                // shown
    ///   hiddenNodes, hiddenEdges,    // removed, or at a removed node
    ///   nodeSlots, edgeSlots,        // the next node's and edge's index
    ///   size: { column, range },     // column null: by degree
    ///   color: { column, entries: [{ category, label, color, count }], other, none } | null,
    ///   frame: { k, nodes, edges, ordered, p95, frames, limit } }  // the frame budget: frameInfo()
    /// ```
    ///
    /// It follows every [`GraphView::apply_delta`]. The node counts cost
    /// nothing; the shown edges are counted (in time proportional to the
    /// edges) only while a node is hidden, and the legend (in time
    /// proportional to the nodes) only when a column colours them, on the
    /// first call after a delta.
    pub fn info(&self) -> Result<JsValue, JsValue> {
        #[derive(serde::Serialize)]
        struct WithFrame {
            #[serde(flatten)]
            info: crate::live::Info,
            frame: crate::view::FrameInfo,
        }
        match self.view.info() {
            Some(info) => {
                let frame = self.view.frame_info();
                let json = serde_json::to_string(&WithFrame { info, frame }).map_err(js_err)?;
                js_sys::JSON::parse(&json)
            }
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// The result drawn, with every delta applied, as UGR; `undefined` for
    /// a view not made from one.
    #[wasm_bindgen(js_name = resultBytes)]
    pub fn result_bytes(&self) -> Option<js_sys::Uint8Array> {
        // Each section straight from the result into the page's array: the
        // buffer is never built in this module's memory.
        let encoded = self.view.result()?.encoded();
        let out = js_sys::Uint8Array::new_with_length(encoded.len() as u32);
        encoded.write(|at, bytes| {
            out.subarray(at as u32, (at + bytes.len()) as u32)
                .copy_from(bytes)
        });
        Some(out)
    }

    /// Draw these positions: `x` then `y`, one per node each, a
    /// `Float32Array` as [`Layout::positions`] gives them.
    ///
    /// The array is copied once, from the page's memory into the positions
    /// the view keeps (for picking, and for the nodes drawn in front), and
    /// those are what is written to the GPU: it never passes through a
    /// second array here.
    #[wasm_bindgen(js_name = setPositions)]
    pub fn set_positions(&mut self, xy: &js_sys::Float32Array) -> Result<(), JsValue> {
        let n = self.view.n_nodes();
        if xy.length() as usize != 2 * n {
            return Err(js_err(format!(
                "expected {} values (x then y); got {}",
                2 * n,
                xy.length()
            )));
        }
        let n = n as u32;
        self.view
            .set_positions_with(&self.device, &self.queue, |x, y| {
                xy.subarray(0, n).copy_to(x);
                xy.subarray(n, 2 * n).copy_to(y);
            });
        Ok(())
    }

    /// Draw a level of the layout's hierarchy in place of the graph
    /// (#497), until [`GraphView::clear_level`]: a disc for each of its
    /// nodes, its area by the node's `mass`, and its merged edges
    /// `(src[i], dst[i])`, each as wide and as dark as its `weight[i]`
    /// says, heaviest first. What [`Layout::level_graph`] gives, for level
    /// `level` of `levels`; `totalEdges` is how many merged edges the
    /// level has, of which these are the heaviest.
    ///
    /// The level's nodes are at the origin until
    /// [`GraphView::set_level_positions`]. While a level is drawn, a frame
    /// is the level's alone, within the frame budget
    /// ([`GraphView::render_frame`]: the heaviest discs and edges while
    /// the view moves); the camera fits the level, if it follows; and
    /// nothing is picked, the graph's nodes having no place yet.
    #[wasm_bindgen(js_name = setLevel)]
    #[allow(clippy::too_many_arguments)]
    pub fn set_level(
        &mut self,
        level: u32,
        levels: u32,
        mass: Vec<f32>,
        src: Vec<u32>,
        dst: Vec<u32>,
        weight: Vec<f32>,
        total_edges: Option<u32>,
    ) -> Result<(), JsValue> {
        let total = total_edges.map_or(src.len(), |t| t as usize);
        let scene = crate::levels::LevelScene::new(
            level as usize,
            levels as usize,
            mass,
            src,
            dst,
            weight,
            total,
        )
        .map_err(js_err)?;
        self.view.set_level(&self.device, self.config.format, scene);
        Ok(())
    }

    /// Put the nodes of the level being drawn at `xy`: `x` then `y`, one
    /// per node of the level each, as [`Layout::level_positions`] gives
    /// them.
    #[wasm_bindgen(js_name = setLevelPositions)]
    pub fn set_level_positions(&mut self, xy: &js_sys::Float32Array) -> Result<(), JsValue> {
        let n = match self.view.level() {
            Some(level) => level.n_nodes(),
            None => return Err(js_err("no level is drawn")),
        };
        if xy.length() as usize != 2 * n {
            return Err(js_err(format!(
                "expected {} values (x then y) for a level of {n} nodes; got {}",
                2 * n,
                xy.length()
            )));
        }
        let n = n as u32;
        self.view.set_level_positions(|x, y| {
            xy.subarray(0, n).copy_to(x);
            xy.subarray(n, 2 * n).copy_to(y);
        });
        Ok(())
    }

    /// Draw the graph again, and let the level go (or the cut:
    /// [`GraphView::set_cut`]).
    #[wasm_bindgen(js_name = clearLevel)]
    pub fn clear_level(&mut self) -> Result<(), JsValue> {
        let cut = self.view.draws_cut();
        self.view.clear_level();
        // The selected nodes' edges, which a cut does not draw.
        if cut && !self.interact.selection().nodes().is_empty() {
            self.draw_selection()?;
        }
        Ok(())
    }

    /// Draw a cut through the layout's hierarchy in place of the graph
    /// (#498), until [`GraphView::clear_level`]: what [`Layout::cut`]
    /// gives. A disc for each of the cut's groups, at `xy` (`x` then `y`,
    /// one per group each), its area by the group's `mass`; `members[i]`
    /// is how many of the graph's nodes group `i` stands for, and `of[v]`
    /// the group node `v` of the graph is in; the merged edges
    /// `(src[i], dst[i])` between groups, heaviest first, each as wide and
    /// as dark as its `weight[i]` says, of the `totalEdges` the cut has.
    /// No disc is drawn with a radius over `maxRadius` pixels, if given:
    /// the discs of a cut stand a known distance apart.
    ///
    /// `kinds`, if given, says how each group is drawn where it is not by
    /// its mass (`GROUP BY`'s groups, #499, one value per group): `0` by
    /// its mass; above 0, an open disc of that radius in the layout's
    /// units, a rim under the others; below 0, a point of that many
    /// pixels' radius over them; NaN, not at all, nor its edges.
    ///
    /// The camera is not moved: the cut is the graph's own drawing,
    /// coarser. A group's disc takes the colour of most of its nodes
    /// ([`GraphView::color_by`]) and carries a ring in the selection
    /// colour when any of them is selected. While a cut is drawn the
    /// graph's nodes and edges are not picked: [`GraphView::pick_group`]
    /// finds a group, and [`GraphView::select_group`] selects its nodes.
    #[wasm_bindgen(js_name = setCut)]
    #[allow(clippy::too_many_arguments)]
    pub fn set_cut(
        &mut self,
        xy: &js_sys::Float32Array,
        mass: Vec<f32>,
        members: Vec<u32>,
        of: Vec<u32>,
        src: Vec<u32>,
        dst: Vec<u32>,
        weight: Vec<f32>,
        total_edges: Option<u32>,
        max_radius: Option<f32>,
        kinds: Option<Vec<f32>>,
    ) -> Result<(), JsValue> {
        let groups = mass.len();
        if xy.length() as usize != 2 * groups {
            return Err(js_err(format!(
                "expected {} values (x then y) for a cut of {groups} groups; got {}",
                2 * groups,
                xy.length()
            )));
        }
        let total = total_edges.map_or(src.len(), |t| t as usize);
        let mut scene =
            crate::levels::LevelScene::new(0, 1, mass, src, dst, weight, total).map_err(js_err)?;
        scene.set_max_radius(max_radius.unwrap_or(0.0));
        scene.set_kinds(kinds.unwrap_or_default()).map_err(js_err)?;
        let (x, y) = scene.positions_mut();
        let n = groups as u32;
        xy.subarray(0, n).copy_to(x);
        xy.subarray(n, 2 * n).copy_to(y);
        let was = self.view.draws_cut();
        self.view
            .set_cut(&self.device, self.config.format, scene, members, of)
            .map_err(js_err)?;
        // A cut draws no edges of the graph's: the selection's are let go.
        if !was && !self.interact.selection().nodes().is_empty() {
            self.draw_selection()?;
        }
        Ok(())
    }

    /// What is drawn of a cut, or `undefined` when none is:
    ///
    /// ```js
    /// { groups, merged,      // its discs, and those of more than one node
    ///   nodes,               // the graph's nodes under them: all of them
    ///   groupsDrawn,         // the discs the last frame drew
    ///   edges, edgesDrawn,   // the merged edges held, and drawn
    ///   totalEdges }         // the merged edges the cut has
    /// ```
    #[wasm_bindgen(js_name = cutInfo)]
    pub fn cut_info(&self) -> Result<JsValue, JsValue> {
        match self.view.cut_info() {
            Some(info) => js_sys::JSON::parse(&serde_json::to_string(&info).map_err(js_err)?),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// The group of the cut drawn under the pixel `(x, y)`, or
    /// `undefined`: the heaviest of those whose disc covers it.
    #[wasm_bindgen(js_name = pickGroup)]
    pub fn pick_group(&self, x: f32, y: f32) -> Option<u32> {
        self.view.pick_group(x, y)
    }

    /// The radius group `group` of the cut drawn has on the canvas now,
    /// in pixels, or `undefined`.
    #[wasm_bindgen(js_name = groupRadius)]
    pub fn group_radius(&self, group: u32) -> Option<f32> {
        self.view.group_radius(group)
    }

    /// The graph's nodes that group `group` of the cut drawn stands for,
    /// ascending.
    #[wasm_bindgen(js_name = groupNodes)]
    pub fn group_nodes(&self, group: u32) -> Vec<u32> {
        self.view.group_nodes(group)
    }

    /// The group of the cut drawn that node `node` is in, or `undefined`.
    #[wasm_bindgen(js_name = groupOf)]
    pub fn group_of(&self, node: u32) -> Option<u32> {
        self.view.group_of(node)
    }

    /// A click, for the selection, while a cut is drawn: on `group` (as
    /// [`GraphView::pick_group`] gave it) or on empty space
    /// (`undefined`), with Shift held (`extend`) or not. As
    /// [`GraphView::select`], with a group's nodes for the node: a plain
    /// click on a group selects its nodes alone, and on empty space
    /// clears the selection; with `extend`, a click on a group adds its
    /// nodes (or takes them out, if all were selected). Returns whether
    /// the selection changed.
    #[wasm_bindgen(js_name = selectGroup)]
    pub fn select_group(&mut self, group: Option<u32>, extend: bool) -> Result<bool, JsValue> {
        let Some(group) = group else {
            return self.select(None, extend);
        };
        let nodes = self.view.group_nodes(group);
        if nodes.is_empty() {
            return Ok(false);
        }
        let edge = self.interact.select_edge(None);
        let selection = self.interact.selection_mut();
        let changed = if !extend {
            selection.set(&nodes)
        } else {
            let mut all: Vec<u32> = selection.nodes().to_vec();
            let has: std::collections::HashSet<u32> = all.iter().copied().collect();
            if nodes.iter().all(|v| has.contains(v)) {
                let out: std::collections::HashSet<u32> = nodes.iter().copied().collect();
                all.retain(|v| !out.contains(v));
            } else {
                all.extend(nodes.iter().filter(|v| !has.contains(v)));
            }
            selection.set(&all)
        };
        if changed || edge {
            self.draw_selection()?;
        }
        Ok(changed || edge)
    }

    /// The labels of the groups of the cut drawn that are large enough on
    /// the canvas to carry one: for each of the at most `most` heaviest
    /// groups of more than one node whose disc is in view with a radius of
    /// at least `minRadius` pixels, `[group, x, y, radius, members]` (in
    /// canvas pixels from the top left), flat, heaviest first.
    #[wasm_bindgen(js_name = groupLabels)]
    pub fn group_labels(&self, min_radius: f32, most: u32) -> Vec<f32> {
        self.view
            .group_labels(min_radius, most as usize)
            .iter()
            .flat_map(|l| [l.group as f32, l.x, l.y, l.radius, l.members as f32])
            .collect()
    }

    /// The camera: `[x, y, pixelsPerUnit, width, height]`, the point of
    /// the layout at the middle of the canvas, how many canvas pixels a
    /// unit of the layout spans, and the canvas's size in pixels. What
    /// [`Layout::cut`] is asked with.
    pub fn camera(&self) -> Vec<f32> {
        let c = self.view.camera();
        vec![
            c.center.0,
            c.center.1,
            c.pixels_per_unit,
            c.viewport.0,
            c.viewport.1,
        ]
    }

    /// What is drawn of a level, or `undefined` when the graph is drawn:
    ///
    /// ```js
    /// { level, levels,       // which level, of how many (the graph is level 0)
    ///   nodes, nodesDrawn,   // its nodes, and how many the last frame drew
    ///   edges, edgesDrawn,   // the merged edges held, and drawn
    ///   totalEdges }         // the merged edges the level has
    /// ```
    #[wasm_bindgen(js_name = levelInfo)]
    pub fn level_info(&self) -> Result<JsValue, JsValue> {
        match self.view.level_info() {
            Some(info) => js_sys::JSON::parse(&serde_json::to_string(&info).map_err(js_err)?),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// The positions now: `x` then `y`, one per node each, in a new
    /// `Float32Array` (one copy, from the view's memory).
    pub fn positions(&self) -> js_sys::Float32Array {
        let (x, y) = self.view.positions();
        positions_array(x, y)
    }

    fn check_len(&self, n: usize) -> Result<(), JsValue> {
        let want = self.view.n_nodes();
        if n != want {
            return Err(js_err(format!(
                "expected one value per node ({want}); got {n}"
            )));
        }
        Ok(())
    }
}

/// What [`GraphView::apply_delta`] did.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy)]
pub struct DeltaStats {
    /// The node and edge slots there are now: the next node's and the next
    /// edge's index. Hidden ones are counted ([`GraphView::info`] has the
    /// shown).
    pub nodes: u32,
    pub edges: u32,
    /// Bytes written to the GPU: 16 for a new node, 8 for an edge, 4 for a
    /// node resized (8 if any was recoloured), 4 for a node whose flags
    /// changed, and the highlight layer's instances.
    #[wasm_bindgen(js_name = bytesUploaded)]
    pub bytes_uploaded: f64,
    /// Whether a buffer doubled to hold it: its contents were copied on the
    /// GPU, not uploaded again.
    pub grew: bool,
    /// The nodes and edges the buffers have room for.
    #[wasm_bindgen(js_name = nodeCapacity)]
    pub node_capacity: u32,
    #[wasm_bindgen(js_name = edgeCapacity)]
    pub edge_capacity: u32,
}

/// The delta that undoes `bytes`, a delta as UGR: each op's inverse, in
/// reverse order. It needs no result and no query.
#[wasm_bindgen(js_name = invertDelta)]
pub fn invert_delta(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    let delta = GraphDelta::from_bytes(bytes).map_err(js_err)?;
    Ok(delta.invert().to_bytes())
}

/// A result as UGR for a page with no engine (the demo, the tests): the
/// graph of `n_nodes` nodes and the edges `(src[i], dst[i])`, its nodes of
/// one label, `node`, keyed `0..n_nodes`, and its edges of one, `edge`.
/// `xy` (`x` then `y`, one per node) gives it positions.
#[wasm_bindgen(js_name = resultFromEdges)]
pub fn result_from_edges(
    n_nodes: u32,
    src: Vec<u32>,
    dst: Vec<u32>,
    xy: Option<Vec<f32>>,
) -> Result<Vec<u8>, JsValue> {
    let n = n_nodes as usize;
    let mut nodes = NodeBatch::of_label(0, 0..i64::from(n_nodes));
    let mut ops = vec![
        Op::AddNodeLabel(NodeLabel::new("node", KeyType::Int)),
        Op::AddEdgeLabel("edge".into()),
    ];
    if let Some(xy) = xy {
        if xy.len() != 2 * n {
            return Err(js_err(format!(
                "expected {} values (x then y); got {}",
                2 * n,
                xy.len()
            )));
        }
        let positions = Positions {
            x: xy[..n].to_vec(),
            y: xy[n..].to_vec(),
        };
        // An empty result takes positions before its nodes do.
        ops.push(Op::SetPositions {
            old: None,
            new: Some(Positions::default()),
        });
        nodes.positions = Some(positions);
    }
    ops.push(Op::AddNodes(nodes));
    ops.push(Op::AddEdges(EdgeBatch::of_label(0, src, dst)));
    GraphResult::build(None, Provenance::default(), &GraphDelta::new(ops))
        .map(|r| r.to_bytes())
        .map_err(js_err)
}

/// Builds a delta as UGR for a page with no engine (the demo, the tests),
/// against a result of [`result_from_edges`]: its ops in the order they
/// are added.
#[wasm_bindgen]
#[derive(Default)]
pub struct DeltaBuilder {
    ops: Vec<Op>,
}

#[wasm_bindgen]
impl DeltaBuilder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    /// `count` new nodes, keyed `first_key..first_key + count`: use the
    /// result's node count, so the keys are new. `xy` (`x` then `y`) puts
    /// them, for a result that has positions.
    #[wasm_bindgen(js_name = addNodes)]
    pub fn add_nodes(
        mut self,
        first_key: u32,
        count: u32,
        xy: Option<Vec<f32>>,
    ) -> Result<DeltaBuilder, JsValue> {
        let keys = i64::from(first_key)..i64::from(first_key) + i64::from(count);
        let mut nodes = NodeBatch::of_label(0, keys);
        if let Some(xy) = xy {
            let n = count as usize;
            if xy.len() != 2 * n {
                return Err(js_err(format!(
                    "expected {} values (x then y); got {}",
                    2 * n,
                    xy.len()
                )));
            }
            nodes.positions = Some(Positions {
                x: xy[..n].to_vec(),
                y: xy[n..].to_vec(),
            });
        }
        self.ops.push(Op::AddNodes(nodes));
        Ok(self)
    }

    /// New edges, `(src[i], dst[i])` by node index.
    #[wasm_bindgen(js_name = addEdges)]
    pub fn add_edges(mut self, src: Vec<u32>, dst: Vec<u32>) -> DeltaBuilder {
        self.ops
            .push(Op::AddEdges(EdgeBatch::of_label(0, src, dst)));
        self
    }

    /// Remove (hide) nodes, by index. They keep their slots.
    #[wasm_bindgen(js_name = removeNodes)]
    pub fn remove_nodes(mut self, nodes: Vec<u32>) -> DeltaBuilder {
        self.ops.push(Op::RemoveNodes(nodes));
        self
    }

    /// Remove edges, by index. They keep their slots.
    #[wasm_bindgen(js_name = removeEdges)]
    pub fn remove_edges(mut self, edges: Vec<u32>) -> DeltaBuilder {
        self.ops.push(Op::RemoveEdges(edges));
        self
    }

    /// The delta as UGR.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(self) -> Vec<u8> {
        GraphDelta::new(self.ops).to_bytes()
    }
}

/// The bits of a node's flags byte ([`GraphView::set_flags`]): or them
/// together.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeFlag {
    Selected = 1,
    Hovered = 2,
    Pinned = 4,
    Dimmed = 8,
    Highlighted = 16,
    Hidden = 32,
    Seed = 64,
}

// The JavaScript enum is the Rust constants.
const _: () = {
    assert!(NodeFlag::Selected as u8 == flag::SELECTED);
    assert!(NodeFlag::Hovered as u8 == flag::HOVERED);
    assert!(NodeFlag::Pinned as u8 == flag::PINNED);
    assert!(NodeFlag::Dimmed as u8 == flag::DIMMED);
    assert!(NodeFlag::Highlighted as u8 == flag::HIGHLIGHTED);
    assert!(NodeFlag::Hidden as u8 == flag::HIDDEN);
    assert!(NodeFlag::Seed as u8 == flag::SEED);
};

/// `x` then `y` in a new `Float32Array`: each copied once, from this
/// module's memory to the page's, with no array between. The page owns the
/// result, and can transfer it.
fn positions_array(x: &[f32], y: &[f32]) -> js_sys::Float32Array {
    let n = x.len() as u32;
    let out = js_sys::Float32Array::new_with_length(2 * n);
    out.subarray(0, n).copy_from(x);
    out.subarray(n, 2 * n).copy_from(y);
    out
}

/// The undirected adjacency of the graph of `n_nodes` nodes and the edges
/// `(src[i], dst[i])`, which is all a layout reads of it; the edge list is
/// freed before the layout is made.
fn adjacency_of(n_nodes: u32, src: Vec<u32>, dst: Vec<u32>) -> Result<Arc<UndirectedCsr>, JsValue> {
    let graph = checked_graph(n_nodes, src, dst)?;
    Ok(Arc::clone(graph.adjacency()))
}

/// An undirected adjacency from its parts, checked
/// ([`UndirectedCsr::from_parts`]).
fn adjacency_from(
    n_nodes: u32,
    offsets: Vec<u64>,
    targets: Vec<u32>,
) -> Result<Arc<UndirectedCsr>, JsValue> {
    UndirectedCsr::from_parts(n_nodes as usize, offsets, targets)
        .map(Arc::new)
        .map_err(js_err)
}

impl From<LiveLayout> for Layout {
    fn from(layout: LiveLayout) -> Self {
        Layout { layout }
    }
}

/// The graph of `n_nodes` nodes and the edges `(src[i], dst[i])`, or why it
/// is not one.
fn checked_graph(n_nodes: u32, src: Vec<u32>, dst: Vec<u32>) -> Result<Graph, JsValue> {
    if src.len() != dst.len() {
        return Err(js_err(format!(
            "src has {} edges but dst has {}",
            src.len(),
            dst.len()
        )));
    }
    Graph::new(n_nodes as usize, src, dst).map_err(js_err)
}

/// The layout alone, with no canvas: for a Web Worker, so a slow iteration
/// never holds up a frame. Post [`Layout::positions`] to the page and give
/// them to [`GraphView::setPositions`].
#[wasm_bindgen]
pub struct Layout {
    layout: LiveLayout,
}

#[wasm_bindgen]
impl Layout {
    /// The layout of the graph of `n_nodes` nodes and the edges
    /// `(src[i], dst[i])`: coarse to fine unless `multilevel` is `false`, in
    /// which case flat ForceAtlas2.
    #[wasm_bindgen(constructor)]
    pub fn new(
        n_nodes: u32,
        src: Vec<u32>,
        dst: Vec<u32>,
        multilevel: Option<bool>,
    ) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        let adjacency = adjacency_of(n_nodes, src, dst)?;
        Ok(Self::on(adjacency, multilevel))
    }

    fn on(adjacency: Arc<UndirectedCsr>, multilevel: Option<bool>) -> Layout {
        let layout = if multilevel.unwrap_or(true) {
            LiveLayout::multilevel_on(adjacency, ursa_core::algo::MultilevelParams::default())
        } else {
            LiveLayout::new_on(adjacency, ursa_core::algo::Fa2Params::default())
        };
        layout.into()
    }

    /// [`Layout::new`] for a caller that holds the graph's undirected
    /// adjacency (UGR's `edges.adjacency.*`, `ursa-viz/UGR.md`) and not
    /// only its edges: node `i`'s neighbours are
    /// `targets[offsets[i]..offsets[i + 1]]`, each list ascending, with no
    /// repeats and no `i`, and every pair in both nodes' lists. `offsets`
    /// is a `BigUint64Array` of `n_nodes + 1`. The adjacency is checked in
    /// one pass and used as it is: nothing is sorted, and no edge list is
    /// held. The layout is the same, bit for bit, as [`Layout::new`]'s of
    /// edges with that adjacency.
    ///
    /// `kind` picks a fixed layout instead, with `level` as
    /// [`Layout::rings`] and [`Layout::layers`] take it: `"rings"` or
    /// `"layers"`.
    #[wasm_bindgen(js_name = fromAdjacency)]
    pub fn from_adjacency(
        n_nodes: u32,
        offsets: Vec<u64>,
        targets: Vec<u32>,
        kind: Option<String>,
        level: Option<Vec<u32>>,
    ) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        let adjacency = adjacency_from(n_nodes, offsets, targets)?;
        let level = level.as_deref().unwrap_or(&[]);
        match kind.as_deref() {
            None | Some("force") => Ok(Self::on(adjacency, None)),
            Some("rings") => {
                let params = ursa_core::algo::RingsParams::default();
                let layout = LiveLayout::rings_on(adjacency, level, params).map_err(js_err)?;
                Ok(layout.into())
            }
            Some("layers") => {
                let params = ursa_core::algo::LayersParams::default();
                let layout = LiveLayout::layers_on(adjacency, level, params).map_err(js_err)?;
                Ok(layout.into())
            }
            Some(other) => Err(js_err(format!("no layout called {other}"))),
        }
    }

    /// A layout of the graph that starts from `xy` (`x` then `y`, one per
    /// node each, as [`Layout::positions`] gives them) and has nothing left
    /// to run: saved positions, or the ones an undo restores. It is there to
    /// [`Layout::grow`] from.
    #[wasm_bindgen(js_name = fromPositions)]
    pub fn from_positions(
        n_nodes: u32,
        src: Vec<u32>,
        dst: Vec<u32>,
        xy: &[f32],
    ) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        let adjacency = adjacency_of(n_nodes, src, dst)?;
        let n = (n_nodes as usize).min(xy.len());
        let (x, y) = xy.split_at(n);
        let params = ursa_core::algo::MultilevelParams::default().fa2;
        let layout = LiveLayout::from_positions_on(adjacency, params, x.to_vec(), y.to_vec())
            .map_err(js_err)?;
        Ok(layout.into())
    }

    /// Concentric rings, for a neighbourhood (a `hop` result): `distance[i]`
    /// is node `i`'s distance from a seed, 0 for a seed and `0xFFFFFFFF` for
    /// a node that has none. One seed is at the centre with distance `d` on
    /// the circle of radius `d`; several seeds get a ring system each, side
    /// by side. The order on each ring shortens the edges. The layout is
    /// [`Layout::done`] at once: read [`Layout::positions`], no stepping.
    /// The same input gives the same bits every time.
    pub fn rings(
        n_nodes: u32,
        src: Vec<u32>,
        dst: Vec<u32>,
        distance: &[u32],
    ) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        let adjacency = adjacency_of(n_nodes, src, dst)?;
        let params = ursa_core::algo::RingsParams::default();
        let layout = LiveLayout::rings_on(adjacency, distance, params).map_err(js_err)?;
        Ok(layout.into())
    }

    /// Layers left to right, for paths: `layer[i]` is node `i`'s position
    /// along its path (or its distance from the nearest of several seeds),
    /// `0xFFFFFFFF` for a node that has none. Layer `l` is the vertical line
    /// at `x = l`, and one path alone is a straight line. Done at once, like
    /// [`Layout::rings`].
    pub fn layers(
        n_nodes: u32,
        src: Vec<u32>,
        dst: Vec<u32>,
        layer: &[u32],
    ) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        let adjacency = adjacency_of(n_nodes, src, dst)?;
        let params = ursa_core::algo::LayersParams::default();
        let layout = LiveLayout::layers_on(adjacency, layer, params).map_err(js_err)?;
        Ok(layout.into())
    }

    /// Which layout this is: `"force"`, `"rings"` or `"layers"`. Rings and
    /// layers that have grown are `"force"`.
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        self.layout.kind().name().to_string()
    }

    /// Grow the layout to the graph of `n_nodes` nodes and the edges
    /// `(src[i], dst[i])`: the graph it had, with the same node indexes,
    /// plus new nodes after them and any new edges.
    ///
    /// Each new node starts beside its parent: `parents[i]` for new node
    /// `oldNodeCount + i`, or, where that is `0xFFFFFFFF` or `parents` is
    /// left out, its lowest-indexed neighbour already drawn. The nodes in
    /// `pinned` do not move at all, nor do the parents, nor the old nodes
    /// more than two hops from a new one; the old nodes nearer than that
    /// move a tenth as readily as the new. A drawing of more than 5,000
    /// nodes is held whole. The schedule is short: [`Layout::total`]
    /// iterations (60 for a small addition), run by [`Layout::step`] until
    /// [`Layout::done`]. The same layout grown by the same additions has the
    /// same bits every time. On an error the layout is as it was.
    pub fn grow(
        &mut self,
        n_nodes: u32,
        src: Vec<u32>,
        dst: Vec<u32>,
        parents: Option<Vec<u32>>,
        pinned: Option<Vec<u32>>,
    ) -> Result<(), JsValue> {
        let adjacency = adjacency_of(n_nodes, src, dst)?;
        self.layout
            .grow_on(
                adjacency,
                Growth {
                    parents: parents.as_deref(),
                    pinned: pinned.as_deref().unwrap_or(&[]),
                    params: ursa_core::algo::WarmParams::default(),
                },
            )
            .map_err(js_err)
    }

    /// Run `iterations` more iterations: coarse to fine, work worth that many
    /// iterations of the whole graph. A grown layout runs no more than its
    /// warm schedule has left.
    pub fn step(&mut self, iterations: u32) {
        self.layout.step(iterations);
    }

    /// [`Layout::step`], a level at a time (#497), for a page that draws
    /// the level being laid out ([`GraphView::set_level`]) and not every
    /// node at its group's place. Coarse to fine, it comes down a level
    /// only at the start of a call, from a level that has had all its
    /// iterations, and stops when the level it is at has had its own, the
    /// rest of the work it was given dropped: after each call
    /// [`Layout::level_positions`] are that level's, and when
    /// [`Layout::level`] has changed, [`Layout::level_graph`] is the new
    /// level. The final positions are the same bits as [`Layout::step`]'s.
    /// Any other layout steps as [`Layout::step`] does.
    #[wasm_bindgen(js_name = stepLevel)]
    pub fn step_level(&mut self, iterations: u32) {
        self.layout.step_level(iterations);
    }

    /// Whether the level being laid out is a coarse one that has had all
    /// its iterations: the next [`Layout::step_level`] comes down from it,
    /// and the hierarchy above is then let go ([`Layout::edge_order`]).
    #[wasm_bindgen(getter, js_name = levelFinished)]
    pub fn level_finished(&self) -> bool {
        self.layout.level_finished()
    }

    /// The work done so far, in iterations of the whole graph:
    /// [`Layout::iterations`], not rounded.
    #[wasm_bindgen(getter)]
    pub fn work(&self) -> f64 {
        self.layout.work()
    }

    /// The positions of the nodes of the level being laid out: `x` then
    /// `y`, one per node of that level each, in a new `Float32Array`. At
    /// level 0 they are [`Layout::positions`].
    #[wasm_bindgen(js_name = levelPositions)]
    pub fn level_positions(&self) -> js_sys::Float32Array {
        let (x, y) = self.layout.level_positions();
        positions_array(x, y)
    }

    /// The level being laid out, as a graph of its own, or `undefined`
    /// for a layout that is not coarse to fine:
    ///
    /// ```js
    /// { level, levels,   // which level, of how many (the graph is level 0)
    ///   mass,            // Float32Array: each node's mass, the summed
    ///                    // degree + 1 of the graph's nodes it stands for
    ///   parents,         // Uint32Array: each node's parent in the level
    ///                    // above (0xFFFFFFFF for none), if stepLevel came
    ///                    // down to this level; given once
    ///   src, dst,        // Uint32Array: the heaviest `mostEdges` merged
    ///   weight,          // edges, heaviest first, and (Float32Array) how
    ///                    // many of the graph's edges each stands for
    ///   totalEdges }     // how many merged edges the level has
    /// ```
    ///
    /// The arrays are new ones, outside this module's memory, the
    /// caller's to transfer. At level 0 the nodes are the graph's, and
    /// `mass` and the edges are empty: the caller has the graph's own
    /// (a node's mass is its degree plus one). This is every
    /// level's part of the hierarchy, in the level's own numbering: what a
    /// cut through it (#498) is made of.
    #[wasm_bindgen(js_name = levelGraph)]
    pub fn level_graph(&mut self, most_edges: u32) -> Result<JsValue, JsValue> {
        let Some(g) = self.layout.level_graph(most_edges as usize) else {
            return Ok(JsValue::UNDEFINED);
        };
        let out = js_sys::Object::new();
        let set = |key: &str, value: JsValue| js_sys::Reflect::set(&out, &key.into(), &value);
        set("level", (g.level as u32).into())?;
        set("levels", (g.levels as u32).into())?;
        set("mass", js_sys::Float32Array::from(&g.mass[..]).into())?;
        if let Some(parents) = &g.parents {
            set("parents", js_sys::Uint32Array::from(&parents[..]).into())?;
        }
        set("src", js_sys::Uint32Array::from(&g.edges.src[..]).into())?;
        set("dst", js_sys::Uint32Array::from(&g.edges.dst[..]).into())?;
        set(
            "weight",
            js_sys::Float32Array::from(&g.edges.weight[..]).into(),
        )?;
        set("totalEdges", (g.edges.total as u32).into())?;
        Ok(out.into())
    }

    /// Whether [`Layout::cut`] has a hierarchy to cut: coarse to fine,
    /// once the layout has reached the graph itself, until it grows.
    #[wasm_bindgen(getter, js_name = hasCut)]
    pub fn has_cut(&self) -> bool {
        self.layout.has_cut()
    }

    /// Make what [`Layout::cut`] reads, ahead of the first one: the
    /// hierarchy as a forest, with the graph's edges in its order (once),
    /// and its groups where the nodes now are. Returns
    /// [`Layout::has_cut`].
    #[wasm_bindgen(js_name = prepareCut)]
    pub fn prepare_cut(&mut self) -> bool {
        self.layout.prepare_cut()
    }

    /// The bytes [`Layout::cut`] holds between calls.
    #[wasm_bindgen(getter, js_name = cutBytes)]
    pub fn cut_bytes(&self) -> f64 {
        self.layout.cut_bytes() as f64
    }

    /// A cut through the layout's hierarchy for a camera (#498,
    /// `ursa_viz::cut`): the groups to draw in place of the graph, where
    /// its nodes are now. `camera` is [`GraphView::camera`]'s
    /// `[x, y, pixelsPerUnit, width, height]`; `separation` (pixels),
    /// `maxNodes`, `maxEdges` and `detailNodes` default to
    /// `ursa_viz::cut`'s. `undefined` unless [`Layout::has_cut`].
    ///
    /// ```js
    /// { detail,            // the graph itself is to be drawn: what is in
    ///                      // view is its own nodes (no edges are given)
    ///   xy,                // Float32Array: x then y, one per group each
    ///   mass,              // Float32Array: the summed degree + 1
    ///   members,           // Uint32Array: the graph's nodes in each group
    ///   of,                // Uint32Array: each node's group
    ///   src, dst, weight,  // the merged edges, heaviest first
    ///   totalEdges,        // how many merged edges the cut has
    ///   inView, membersInView }
    /// ```
    ///
    /// The arrays are new ones, outside this module's memory, the
    /// caller's to transfer.
    pub fn cut(
        &mut self,
        camera: &[f32],
        separation: Option<f32>,
        max_nodes: Option<u32>,
        max_edges: Option<u32>,
        detail_nodes: Option<u32>,
    ) -> Result<JsValue, JsValue> {
        let &[x, y, ppu, w, h] = camera else {
            return Err(js_err("a camera is [x, y, pixelsPerUnit, width, height]"));
        };
        let mut params = crate::cut::CutParams::of_camera((x, y), ppu, (w, h));
        if let Some(s) = separation {
            params.separation = s;
        }
        if let Some(n) = max_nodes {
            params.max_nodes = n as usize;
        }
        if let Some(m) = max_edges {
            params.max_edges = m as usize;
        }
        if let Some(d) = detail_nodes {
            params.detail_nodes = d as usize;
        }
        let Some(cut) = self.layout.cut(&params) else {
            return Ok(JsValue::UNDEFINED);
        };
        let out = js_sys::Object::new();
        let set = |key: &str, value: JsValue| js_sys::Reflect::set(&out, &key.into(), &value);
        set("detail", cut.detail.into())?;
        set("xy", positions_array(&cut.x, &cut.y).into())?;
        set("mass", js_sys::Float32Array::from(&cut.mass[..]).into())?;
        set(
            "members",
            js_sys::Uint32Array::from(&cut.members[..]).into(),
        )?;
        set("of", js_sys::Uint32Array::from(&cut.of[..]).into())?;
        set("src", js_sys::Uint32Array::from(&cut.src[..]).into())?;
        set("dst", js_sys::Uint32Array::from(&cut.dst[..]).into())?;
        set("weight", js_sys::Float32Array::from(&cut.weight[..]).into())?;
        set("totalEdges", (cut.total_edges as u32).into())?;
        set("inView", (cut.in_view as u32).into())?;
        set("membersInView", (cut.members_in_view as u32).into())?;
        Ok(out.into())
    }

    /// Iterations so far: coarse to fine, work done, in iterations of the
    /// whole graph.
    #[wasm_bindgen(getter)]
    pub fn iterations(&self) -> u32 {
        self.layout.iterations()
    }

    /// The whole schedule's work, in iterations of the whole graph, coarse to
    /// fine; `undefined` for flat, which runs as long as it is stepped; 0 for
    /// rings and layers.
    #[wasm_bindgen(getter)]
    pub fn total(&self) -> Option<u32> {
        self.layout.total()
    }

    /// Whether the layout has finished its schedule: never, for flat; from
    /// the start, for rings and layers.
    #[wasm_bindgen(getter)]
    pub fn done(&self) -> bool {
        self.layout.is_done()
    }

    /// The level laid out now: 0 is the graph itself.
    #[wasm_bindgen(getter)]
    pub fn level(&self) -> u32 {
        self.layout.level() as u32
    }

    /// How many levels there are: the coarse levels, then the graph itself
    /// (level 0). Coarse to fine counts [`Layout::level`] down from
    /// `levels - 1`; flat has one.
    #[wasm_bindgen(getter)]
    pub fn levels(&self) -> u32 {
        self.layout.levels() as u32
    }

    /// The order to draw the edges `(src[i], dst[i])` in, most important
    /// first, for [`GraphView::set_edge_order`]: `order[i]` is the edge to
    /// draw `i`-th. The edges are the layout's graph's, numbered as the
    /// view numbers them. Coarse to fine, the edges between the drawing's
    /// far groups come first (by the level of the hierarchy at which an
    /// edge's two ends merge, then one edge of each merged edge a round,
    /// the heaviest first, then a hash); otherwise the edges at the nodes
    /// of highest degree do. The same input gives the same order.
    ///
    /// `src` and `dst` are `Uint32Array`s and stay the caller's: they are
    /// read a run at a time. The keys the edges are sorted by (eight bytes
    /// an edge) are kept in a `BigUint64Array` of the caller's realm, not
    /// in this module's memory, sorted there, and read back a run at a
    /// time into the `Uint32Array` returned. So taking the order costs
    /// this module two bytes for each adjacency entry of the layout's
    /// largest level and a few runs (#501): a worker's WebAssembly memory
    /// does not grow by the order. Take it before the layout is stepped
    /// down a level: after that the hierarchy above is let go, and the
    /// order is by degree.
    #[wasm_bindgen(js_name = edgeOrder)]
    pub fn edge_order(
        &self,
        src: &js_sys::Uint32Array,
        dst: &js_sys::Uint32Array,
    ) -> Result<js_sys::Uint32Array, JsValue> {
        if src.length() != dst.length() {
            return Err(js_err(format!(
                "src has {} edges but dst has {}",
                src.length(),
                dst.length()
            )));
        }
        let edges = src.length();
        let mut read = |start: usize, s: &mut [u32], d: &mut [u32]| {
            let (from, to) = (start as u32, (start + s.len()) as u32);
            src.subarray(from, to).copy_to(s);
            dst.subarray(from, to).copy_to(d);
        };
        let mut keys = PageKeys(js_sys::BigUint64Array::new_with_length(edges));
        self.layout
            .edge_order_keys(edges as usize, &mut read, &mut keys);
        // Ascending, as unsigned 64-bit integers: a typed array's own sort.
        let sort: js_sys::Function = js_sys::Reflect::get(&keys.0, &"sort".into())?.dyn_into()?;
        sort.call0(&keys.0)?;
        let out = js_sys::Uint32Array::new_with_length(edges);
        let run = ORDER_RUN.min(edges);
        let mut sorted = vec![0u64; run as usize];
        let mut order = vec![0u32; run as usize];
        let mut start = 0;
        while start < edges {
            let len = run.min(edges - start);
            let (sorted, order) = (&mut sorted[..len as usize], &mut order[..len as usize]);
            keys.0.subarray(start, start + len).copy_to(sorted);
            for (edge, key) in order.iter_mut().zip(sorted.iter()) {
                *edge = ursa_core::algo::edge_of_key(*key);
            }
            out.subarray(start, start + len).copy_from(order);
            start += len;
        }
        Ok(out)
    }

    /// The positions now: `x` then `y`, one per node each, in a new
    /// `Float32Array`. They are copied once, from the layout's state into
    /// the array, which is the caller's to transfer to the page:
    /// `postMessage({xy}, [xy.buffer])`, then
    /// [`GraphView::set_positions`], which copies them once more. No other
    /// copy is made on the way to the GPU. (After [`Layout::step_level`]
    /// left a coarse level's positions unprolonged, they are prolonged
    /// here first.)
    pub fn positions(&mut self) -> js_sys::Float32Array {
        self.layout.prolong();
        let (x, y) = self.layout.positions();
        positions_array(x, y)
    }
}
