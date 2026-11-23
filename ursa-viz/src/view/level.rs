//! The view drawing a level of the layout's hierarchy in place of the
//! graph ([`crate::levels`], #497): what [`View::set_level`] starts and
//! [`View::clear_level`] ends.
//!
//! While a level is set, a frame is the level's and nothing of the graph's
//! own is drawn, fitted or picked: its nodes have no positions yet. The
//! frame budget ([`crate::budget`]) is the same one, in the same units: a
//! disc is a quad, as a node is, and so is a merged edge.

use crate::budget::{split, total_units, Frame, NODE_UNITS};
use crate::level_render::LevelRenderer;
use crate::levels::{LevelInfo, LevelScene};

use super::View;

/// A level being drawn.
pub(super) struct Level {
    scene: LevelScene,
    gpu: LevelRenderer,
    /// How many discs and links the last frame drew.
    drew: (usize, usize),
}

impl View {
    /// Draw `scene`, a level of the layout's hierarchy, in place of the
    /// graph, until [`View::clear_level`]: its nodes wherever
    /// [`View::set_level_positions`] last put a level's (the origin, for
    /// the first). `format` is the format frames are drawn in.
    pub fn set_level(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        scene: LevelScene,
    ) {
        self.moved();
        match &mut self.level {
            Some(level) => level.scene = scene,
            None => {
                self.level = Some(Level {
                    scene,
                    gpu: LevelRenderer::new(device, format),
                    drew: (0, 0),
                })
            }
        }
    }

    /// Put the nodes of the level being drawn where `fill(x, y)` writes
    /// them (one per node each), and fit the camera to them if it follows.
    /// Nothing, if no level is set.
    pub fn set_level_positions(&mut self, fill: impl FnOnce(&mut [f32], &mut [f32])) {
        let Some(level) = &mut self.level else {
            return;
        };
        let (x, y) = level.scene.positions_mut();
        fill(x, y);
        self.moved();
        if self.follow {
            self.fit_camera();
        }
    }

    /// Draw the graph again: the level, and what the GPU held of it, are
    /// let go. The graph's nodes are where [`View::set_positions`] put
    /// them, or will.
    pub fn clear_level(&mut self) {
        if self.level.take().is_some() {
            self.moved();
            self.fitted = None;
        }
    }

    /// The level being drawn, if one is.
    pub fn level(&self) -> Option<&LevelScene> {
        self.level.as_ref().map(|l| &l.scene)
    }

    /// What is being drawn of a level, if one is set.
    pub fn level_info(&self) -> Option<LevelInfo> {
        self.level.as_ref().map(|l| LevelInfo {
            level: l.scene.level(),
            levels: l.scene.levels(),
            nodes: l.scene.n_nodes(),
            nodes_drawn: l.drew.0,
            edges: l.scene.n_edges(),
            edges_drawn: l.drew.1,
            total_edges: l.scene.total_edges(),
        })
    }

    /// The box around the level's nodes, if a level is set and has any.
    pub(super) fn level_bounds(&self) -> Option<Option<(f32, f32, f32, f32)>> {
        self.level.as_ref().map(|l| l.scene.bounds())
    }

    /// Give the GPU the `discs` heaviest nodes and the `links` heaviest
    /// edges of the level, as they are now.
    pub(super) fn gather_level(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        discs: usize,
        links: usize,
    ) {
        let (w, h) = self.camera.viewport;
        // As the graph's nodes do: larger as the view zooms in past the fit.
        let zoom = (self.camera.pixels_per_unit / self.fitted_zoom)
            .max(1.0)
            .sqrt();
        let color = self.theme.node;
        if let Some(level) = &mut self.level {
            let d = level.scene.discs(discs, w, h, zoom, color);
            let l = level.scene.links(links);
            level.gpu.set(device, queue, &d, &l);
            level.drew = (d.len(), l.len());
        }
    }

    /// Everything of the level, to the GPU: for a frame that draws it all.
    pub(super) fn prepare_level(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.gather_level(device, queue, usize::MAX, usize::MAX);
    }

    /// [`View::plan_frame`] of the level: within the frame budget while
    /// the view moves, the heaviest discs and the heaviest edges, given to
    /// the GPU for [`View::draw_level`]. A level is never refined in
    /// slices: at rest a frame has all of it.
    pub(super) fn plan_level_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        now_ms: f64,
    ) -> Frame {
        let (n, m) = self
            .level
            .as_ref()
            .map_or((0, 0), |l| (l.scene.n_nodes(), l.scene.n_edges()));
        // A merged edge is a quad, as a node is.
        let frame = self.budget.frame(now_ms, total_units(n, NODE_UNITS * m));
        let (discs, units) = split(frame.units, n, NODE_UNITS * m);
        let links = if units >= NODE_UNITS * m {
            m
        } else {
            (units / NODE_UNITS).max(1).min(m)
        };
        self.gather_level(device, queue, discs, links);
        frame
    }

    /// Draw what the GPU holds of the level into `target`.
    pub(super) fn draw_level(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
    ) {
        if let Some(level) = &self.level {
            level.gpu.draw(
                device,
                queue,
                target,
                &self.camera,
                self.renderer.background,
                self.theme.edge,
            );
        }
    }
}
