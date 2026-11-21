//! A scene and its renderer together: what a platform binding drives.
//!
//! The binding owns the window or canvas, the device and the frame loop; it
//! calls [`View::step`] to advance the layout and [`View::draw`] to draw a
//! frame into a texture. `web` is one such binding; a native window or a
//! notebook widget would be others.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

use ursa_core::algo::Fa2Params;

use crate::highlight::{flag, BadHighlight, HighlightState, MIN_FRONT_RADIUS};
use crate::live::{Changes, Drawn, Info, LiveResult, Parts};
use crate::model::{GraphDelta, GraphResult, ModelError, Op};
use crate::render::{node_radius_scale, FrontNode, Renderer};
use crate::scene::{bounds, Camera, Graph, Growth, LiveLayout, NodeStyle, Theme};

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
}

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
        let highlight = HighlightState::new(graph.n_nodes());
        let mut view = Self {
            graph,
            layout: None,
            x,
            y,
            style,
            theme: Theme::light(),
            categories: None,
            highlight,
            front: Vec::new(),
            camera: Camera::new(width as f32, height as f32),
            renderer,
            follow: true,
            fitted_zoom: 1.0,
            draw_nodes: true,
            live: None,
            layout_stale: false,
        };
        view.fit();
        view
    }

    /// A view of `result`: its nodes where it puts them (or where a layout
    /// would start them), sized and coloured by its encodings (by degree,
    /// without), its paths and seeds highlighted and its removed nodes
    /// hidden. [`View::apply`] then changes it in place.
    pub fn from_result(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        result: GraphResult,
        width: u32,
        height: u32,
    ) -> Self {
        let theme = Theme::light();
        let (live, drawn) = LiveResult::new(result, &theme);
        let Drawn {
            graph,
            x,
            y,
            style,
            categories,
            highlight,
        } = drawn;
        let lines = live.line_indices(0..graph.n_edges());
        let renderer = Renderer::new(device, format, &x, &y, &style, &lines);
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
            live: Some(live),
            layout_stale: false,
        };
        view.upload_flags(device, queue, false);
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
                .write_styles(queue, from, &radius[from..], &color[from..]);
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
            bytes += self.renderer.write_styles(
                queue,
                run.start,
                &radius[run.clone()],
                &color[run.clone()],
            );
        }
        let new_edges = changes.new_edges_from..m1;
        let runs = std::iter::once(&new_edges).chain(&changes.edge_runs);
        for run in runs {
            let lines = live.line_indices(run.clone());
            bytes += self.renderer.write_edges(queue, run.start, &lines);
        }
        // The highlight layer holds positions, radii and colours of its own.
        let layer =
            changes.positions || changes.highlights || !changes.restyled.is_empty() || n1 < n0;
        bytes += self.upload_flags(device, queue, layer);

        if (n0, m0) != (n1, m1) {
            self.layout_stale = true;
        }
        if self.follow && (n0 != n1 || changes.positions) {
            self.fit_camera();
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
        // The highlighted edges and the nodes in front hold positions.
        self.upload_highlight(device, queue, true, true);
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
            || flags_changed && (next.any_hidden() || self.highlight.any_hidden());
        self.highlight = next;
        let dirty = self.highlight.take_dirty();
        self.renderer.set_flags(queue, &self.highlight, &dirty);
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
        if edges && !(self.highlight.edges().is_empty() && self.renderer.highlight_edges() == 0) {
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

    /// Node `i`'s radius as drawn, in pixels, with nodes shrunk by `scale`
    /// ([`View::radius_scale`]): a node in front is never tiny.
    fn drawn_radius(&self, i: usize, scale: f32) -> f32 {
        let r = self.style.radius[i] * scale;
        if self.highlight.flags()[i] & flag::FRONT != 0 {
            r.max(MIN_FRONT_RADIUS)
        } else {
            r
        }
    }

    /// Where node `i` is drawn, in pixels from the canvas's top left.
    pub fn node_pixel(&self, i: usize) -> (f32, f32) {
        self.camera.to_pixel(self.x[i], self.y[i])
    }

    pub fn set_style(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, style: NodeStyle) {
        assert_eq!(style.radius.len(), self.graph.n_nodes());
        assert_eq!(style.color.len(), self.graph.n_nodes());
        self.renderer.set_style(queue, &style);
        self.style = style;
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
    /// whose disc covers it. A hidden node is not there to pick.
    pub fn pick(&self, px: f32, py: f32) -> Option<u32> {
        let (x, y) = (&self.x, &self.y);
        let scale = self.radius_scale();
        let mut best: Option<(u32, f32)> = None;
        let flags = self.highlight.flags();
        for i in 0..x.len() {
            if flags[i] & flag::HIDDEN != 0 {
                continue;
            }
            let (qx, qy) = self.camera.to_pixel(x[i], y[i]);
            let d2 = (qx - px).powi(2) + (qy - py).powi(2);
            let r = self.drawn_radius(i, scale) + 2.0;
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
