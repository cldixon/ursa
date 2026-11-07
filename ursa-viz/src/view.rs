//! A scene and its renderer together: what a platform binding drives.
//!
//! The binding owns the window or canvas, the device and the frame loop; it
//! calls [`View::step`] to advance the layout and [`View::draw`] to draw a
//! frame into a texture. `web` is one such binding; a native window or a
//! notebook widget would be others.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

use ursa_core::algo::Fa2Params;

use crate::render::{node_radius_scale, Renderer};
use crate::scene::{bounds, Camera, Graph, LiveLayout, NodeStyle};

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
    camera: Camera,
    renderer: Renderer,
    /// Fit the camera to the layout as it moves, until the user pans or zooms.
    follow: bool,
    /// The camera's zoom when last fitted: nodes grow back toward full size
    /// as the user zooms in past it.
    fitted_zoom: f32,
    /// Draw nodes as well as edges.
    pub draw_nodes: bool,
}

impl View {
    /// A view of `graph` drawn into `format` textures of `width` x `height`
    /// pixels. Nodes are sized by degree until [`View::set_style`].
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        graph: Graph,
        width: u32,
        height: u32,
    ) -> Self {
        // The seeded positions, as the layout starts from them.
        let start = LiveLayout::new(&graph, Fa2Params::default());
        let (x, y) = (start.positions().0.to_vec(), start.positions().1.to_vec());
        drop(start);
        let style = NodeStyle::by_degree(&graph.degrees());
        let renderer = Renderer::new(device, format, &x, &y, &style, &graph.line_indices());
        let mut view = Self {
            graph,
            layout: None,
            x,
            y,
            style,
            camera: Camera::new(width as f32, height as f32),
            renderer,
            follow: true,
            fitted_zoom: 1.0,
            draw_nodes: true,
        };
        view.fit();
        view
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

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Run `iterations` more layout iterations and upload the new positions.
    pub fn step(&mut self, queue: &wgpu::Queue, iterations: u32) {
        if iterations == 0 {
            return;
        }
        let graph = &self.graph;
        let layout = self
            .layout
            .get_or_insert_with(|| LiveLayout::new(graph, Fa2Params::default()));
        layout.step(iterations);
        let (x, y) = layout.positions();
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
        self.upload(queue);
    }

    /// Draw these positions (one per node), from a layout run elsewhere.
    pub fn set_positions(&mut self, queue: &wgpu::Queue, x: &[f32], y: &[f32]) {
        assert_eq!(x.len(), self.graph.n_nodes());
        assert_eq!(y.len(), self.graph.n_nodes());
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
        self.upload(queue);
    }

    fn upload(&mut self, queue: &wgpu::Queue) {
        self.renderer.set_positions(queue, &self.x, &self.y);
        if self.follow {
            self.fit_camera();
        }
    }

    pub fn set_style(&mut self, queue: &wgpu::Queue, style: NodeStyle) {
        assert_eq!(style.radius.len(), self.graph.n_nodes());
        assert_eq!(style.color.len(), self.graph.n_nodes());
        self.renderer.set_style(queue, &style);
        self.style = style;
    }

    pub fn style(&self) -> &NodeStyle {
        &self.style
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.camera.viewport = (width.max(1) as f32, height.max(1) as f32);
        if self.follow {
            self.fit_camera();
        }
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.follow = false;
        self.camera.pan(dx, dy);
    }

    pub fn zoom(&mut self, factor: f32, px: f32, py: f32) {
        self.follow = false;
        self.camera.zoom(factor, px, py);
    }

    /// Fit the whole layout in view, and keep it fitted as it moves.
    pub fn fit(&mut self) {
        self.follow = true;
        self.fit_camera();
    }

    fn fit_camera(&mut self) {
        if let Some(b) = bounds(&self.x, &self.y) {
            self.camera.fit(b);
            self.fitted_zoom = self.camera.pixels_per_unit;
        }
    }

    /// The node drawn under the pixel `(px, py)`, if any: the nearest node
    /// whose disc covers it.
    pub fn pick(&self, px: f32, py: f32) -> Option<u32> {
        let (x, y) = (&self.x, &self.y);
        let scale = self.radius_scale();
        let mut best: Option<(u32, f32)> = None;
        for i in 0..x.len() {
            let (qx, qy) = self.camera.to_pixel(x[i], y[i]);
            let d2 = (qx - px).powi(2) + (qy - py).powi(2);
            let r = self.style.radius[i] * scale + 2.0;
            if d2 <= r * r && best.is_none_or(|(_, b)| d2 < b) {
                best = Some((i as u32, d2));
            }
        }
        best.map(|(i, _)| i)
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

/// Give the event loop a turn: a macrotask in the browser, where GPU
/// callbacks are delivered; natively, nothing to wait for.
async fn yield_now() {
    #[cfg(target_arch = "wasm32")]
    {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let global = js_sys::global();
            let set_timeout = js_sys::Reflect::get(&global, &"setTimeout".into())
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
            match set_timeout {
                Some(f) => {
                    let _ = f.call2(&global, &resolve, &0.into());
                }
                None => {
                    let _ = resolve.call0(&wasm_bindgen::JsValue::UNDEFINED);
                }
            }
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}
