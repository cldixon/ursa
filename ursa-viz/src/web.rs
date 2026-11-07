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

use crate::scene::{Graph, LiveLayout, NodeStyle};
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

    /// Node colours from a categorical column, one category per node.
    #[wasm_bindgen(js_name = colorBy)]
    pub fn color_by(&mut self, categories: Vec<u32>) -> Result<(), JsValue> {
        self.check_len(categories.len())?;
        let mut style = self.view.style().clone();
        style.color = NodeStyle::colors_from(&categories);
        self.view.set_style(&self.queue, style);
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

/// The layout alone, with no canvas: for a Web Worker, so a slow iteration
/// never holds up a frame. Post [`Layout::positions`] to the page and give
/// them to [`GraphView::setPositions`].
#[wasm_bindgen]
pub struct Layout {
    layout: LiveLayout,
}

#[wasm_bindgen]
impl Layout {
    #[wasm_bindgen(constructor)]
    pub fn new(n_nodes: u32, src: Vec<u32>, dst: Vec<u32>) -> Result<Layout, JsValue> {
        console_error_panic_hook::set_once();
        if src.len() != dst.len() {
            return Err(js_err(format!(
                "src has {} edges but dst has {}",
                src.len(),
                dst.len()
            )));
        }
        let graph = Graph::new(n_nodes as usize, src, dst).map_err(js_err)?;
        Ok(Layout {
            layout: LiveLayout::new(&graph, ursa_core::algo::Fa2Params::default()),
        })
    }

    /// Run `iterations` more iterations.
    pub fn step(&mut self, iterations: u32) {
        self.layout.step(iterations);
    }

    #[wasm_bindgen(getter)]
    pub fn iterations(&self) -> u32 {
        self.layout.iterations()
    }

    /// The positions now: `x` then `y`, one per node each.
    pub fn positions(&self) -> Vec<f32> {
        let (x, y) = self.layout.positions();
        x.iter().chain(y).copied().collect()
    }
}
