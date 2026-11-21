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
    let view = View::new(&device, config.format, graph, width, height);
    Ok(GraphView {
        view,
        _instance: instance,
        surface,
        config,
        device,
        queue,
        backend,
    })
}

#[wasm_bindgen]
impl GraphView {
    /// `"BrowserWebGpu"` or `"Gl"` (WebGL2).
    #[wasm_bindgen(getter)]
    pub fn backend(&self) -> String {
        self.backend.clone()
    }

    #[wasm_bindgen(getter, js_name = nodeCount)]
    pub fn node_count(&self) -> u32 {
        self.view.graph().n_nodes() as u32
    }

    #[wasm_bindgen(getter, js_name = edgeCount)]
    pub fn edge_count(&self) -> u32 {
        self.view.graph().n_edges() as u32
    }

    /// Layout iterations run so far.
    #[wasm_bindgen(getter)]
    pub fn iterations(&self) -> u32 {
        self.view.iterations()
    }

    /// Run `iterations` more layout iterations.
    pub fn step(&mut self, iterations: u32) {
        self.view.step(&self.queue, iterations);
    }

    /// Draw a frame.
    pub fn render(&mut self) -> Result<(), JsValue> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            other => return Err(js_err(format!("no frame to draw on: {other:?}"))),
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.view.draw(&self.device, &self.queue, &target);
        frame.present();
        Ok(())
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

    /// The node under the pixel `(x, y)`, or `undefined`.
    pub fn pick(&self, x: f32, y: f32) -> Option<u32> {
        self.view.pick(x, y)
    }

    /// Node radii from a numeric column, one value per node.
    #[wasm_bindgen(js_name = sizeBy)]
    pub fn size_by(&mut self, values: Vec<f32>, min_px: f32, max_px: f32) -> Result<(), JsValue> {
        self.check_len(values.len())?;
        let mut style = self.view.style().clone();
        style.radius = NodeStyle::sizes_from(&values, min_px, max_px);
        self.view.set_style(&self.queue, style);
        Ok(())
    }

    /// Node colours from a categorical column, one category per node:
    /// category `c` is the theme's `palette[c % palette.length]`. Kept, so
    /// [`GraphView::set_theme`] recolours the nodes.
    #[wasm_bindgen(js_name = colorBy)]
    pub fn color_by(&mut self, categories: Vec<u32>) -> Result<(), JsValue> {
        self.check_len(categories.len())?;
        self.view.color_by(&self.queue, Some(categories));
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
        self.view.set_theme(&self.queue, next);
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
        self.view.set_theme(&self.queue, theme);
        Ok(())
    }

    /// Draw nodes as well as edges (on by default).
    #[wasm_bindgen(setter, js_name = drawNodes)]
    pub fn set_draw_nodes(&mut self, on: bool) {
        self.view.draw_nodes = on;
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
    /// colour. An empty array selects none.
    #[wasm_bindgen(js_name = setSelection)]
    pub fn set_selection(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::SELECTED, nodes))
    }

    /// The node under the pointer, with a thin ring, or none (`undefined`).
    #[wasm_bindgen(js_name = setHovered)]
    pub fn set_hovered(&mut self, node: Option<u32>) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::HOVERED, node.as_slice()))
    }

    /// The pinned nodes: exactly these, each with a small ring in the pin
    /// colour. The view only draws the pin; holding the node in place is
    /// the layout's ([`Layout::grow`]'s `pinned`).
    #[wasm_bindgen(js_name = setPinned)]
    pub fn set_pinned(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::PINNED, nodes))
    }

    /// The hidden nodes: exactly these. A hidden node is not drawn, nor
    /// are its edges, and it cannot be picked; it keeps its index.
    #[wasm_bindgen(js_name = setHidden)]
    pub fn set_hidden(&mut self, nodes: &[u32]) -> Result<(), JsValue> {
        self.highlight(|h| h.set_only(flag::HIDDEN, nodes))
    }

    /// Bytes of flags written to the GPU so far: 4 for each node whose
    /// flags changed.
    #[wasm_bindgen(getter, js_name = flagBytesUploaded)]
    pub fn flag_bytes_uploaded(&self) -> f64 {
        self.view.renderer().flag_bytes_uploaded() as f64
    }

    /// Bytes of highlighted edges written to the GPU so far: 16 for each
    /// edge, each time the edges or the positions change.
    #[wasm_bindgen(getter, js_name = highlightBytesUploaded)]
    pub fn highlight_bytes_uploaded(&self) -> f64 {
        self.view.renderer().highlight_bytes_uploaded() as f64
    }

    /// Where node `node` is drawn: `[x, y]` in canvas pixels from the top
    /// left.
    #[wasm_bindgen(js_name = nodePixel)]
    pub fn node_pixel(&self, node: u32) -> Result<Vec<f32>, JsValue> {
        let n = self.view.graph().n_nodes();
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

    /// Draw these positions: `x` then `y`, one per node each, as
    /// [`Layout::positions`] gives them.
    #[wasm_bindgen(js_name = setPositions)]
    pub fn set_positions(&mut self, xy: &[f32]) -> Result<(), JsValue> {
        let n = self.view.graph().n_nodes();
        if xy.len() != 2 * n {
            return Err(js_err(format!(
                "expected {} values (x then y); got {}",
                2 * n,
                xy.len()
            )));
        }
        self.view.set_positions(&self.queue, &xy[..n], &xy[n..]);
        Ok(())
    }

    /// The positions now: `x` then `y`, one per node each.
    pub fn positions(&self) -> Vec<f32> {
        let (x, y) = self.view.positions();
        x.iter().chain(y).copied().collect()
    }

    fn check_len(&self, n: usize) -> Result<(), JsValue> {
        let want = self.view.graph().n_nodes();
        if n != want {
            return Err(js_err(format!(
                "expected one value per node ({want}); got {n}"
            )));
        }
        Ok(())
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
        let graph = checked_graph(n_nodes, src, dst)?;
        let layout = if multilevel.unwrap_or(true) {
            LiveLayout::multilevel(&graph, ursa_core::algo::MultilevelParams::default())
        } else {
            LiveLayout::new(&graph, ursa_core::algo::Fa2Params::default())
        };
        Ok(Layout { layout })
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
        let graph = checked_graph(n_nodes, src, dst)?;
        let n = (n_nodes as usize).min(xy.len());
        let (x, y) = xy.split_at(n);
        let params = ursa_core::algo::MultilevelParams::default().fa2;
        let layout =
            LiveLayout::from_positions(&graph, params, x.to_vec(), y.to_vec()).map_err(js_err)?;
        Ok(Layout { layout })
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
        let graph = checked_graph(n_nodes, src, dst)?;
        self.layout
            .grow(
                &graph,
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

    /// Iterations so far: coarse to fine, work done, in iterations of the
    /// whole graph.
    #[wasm_bindgen(getter)]
    pub fn iterations(&self) -> u32 {
        self.layout.iterations()
    }

    /// The whole schedule's work, in iterations of the whole graph, coarse to
    /// fine; `undefined` for flat, which runs as long as it is stepped.
    #[wasm_bindgen(getter)]
    pub fn total(&self) -> Option<u32> {
        self.layout.total()
    }

    /// Whether coarse to fine has finished its schedule (never, for flat).
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

    /// The positions now: `x` then `y`, one per node each.
    pub fn positions(&self) -> Vec<f32> {
        let (x, y) = self.layout.positions();
        x.iter().chain(y).copied().collect()
    }
}
