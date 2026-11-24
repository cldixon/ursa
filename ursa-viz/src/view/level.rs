//! The view drawing a level of the layout's hierarchy in place of the
//! graph ([`crate::levels`], #497): what [`View::set_level`] starts and
//! [`View::clear_level`] ends.
//!
//! While a level is set, a frame is the level's and nothing of the graph's
//! own is drawn, fitted or picked: its nodes have no positions yet. The
//! frame budget ([`crate::budget`]) is the same one, in the same units: a
//! disc is a quad, as a node is, and so is a merged edge.

//!
//! A **cut** through the hierarchy (#498, [`crate::cut`]) is drawn the
//! same way, once the layout has reached the graph: [`View::set_cut`]. Its
//! discs are groups of the graph's nodes, which the view knows
//! ([`CutGroups`]): a disc takes the colour of most of its nodes, carries
//! a ring when any of them is selected, and can be picked
//! ([`View::pick_group`]) for its nodes ([`View::group_nodes`]). The camera
//! stays the graph's: a cut is the same drawing, coarser.

use crate::budget::{split, total_units, Frame, NODE_UNITS};
use crate::highlight::flag;
use crate::level_render::LevelRenderer;
use crate::levels::{LevelInfo, LevelScene};
use crate::pace::{ready, Pace};
use crate::scene::NO_CATEGORY;

use super::View;

/// A level being drawn, or the pipelines that will draw one
/// ([`View::warm_levels`]).
pub(super) struct Level {
    scene: Option<LevelScene>,
    gpu: LevelRenderer,
    /// How many discs and links the last frame drew.
    drew: (usize, usize),
    /// The groups the scene's nodes are, if it is a cut.
    cut: Option<CutGroups>,
}

/// The groups of a cut: which of the graph's nodes each disc stands for.
struct CutGroups {
    /// For each node of the graph, its group: a node of the scene.
    of: Vec<u32>,
    /// How many of the graph's nodes each group has.
    members: Vec<u32>,
    /// The category most of a group's nodes have ([`NO_CATEGORY`] with no
    /// categories), the lowest of those that tie.
    category: Vec<u32>,
}

/// What is drawn of a cut ([`View::cut_info`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CutInfo {
    /// The cut's discs, and how many of them stand for more than one of
    /// the graph's nodes.
    pub groups: usize,
    pub merged: usize,
    /// The graph's nodes under them: all of them.
    pub nodes: usize,
    /// The discs the last frame drew.
    pub groups_drawn: usize,
    /// The merged edges held (the heaviest of `total_edges`), and how
    /// many the last frame drew.
    pub edges: usize,
    pub edges_drawn: usize,
    pub total_edges: usize,
}

/// A group's label: where its disc is on the canvas, in pixels, how large,
/// and how many nodes it stands for ([`View::group_labels`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupLabel {
    pub group: u32,
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub members: u32,
}

/// The category most of each group's nodes have: `of[v]` is node `v`'s
/// group, of `groups`, and `categories[v]` its category. The lowest of
/// those that tie.
fn majority(of: &[u32], groups: usize, categories: &[u32]) -> Vec<u32> {
    // (group, category) of every node, sorted: each group's categories
    // are runs, in ascending order.
    let mut pairs: Vec<(u32, u32)> = of.iter().copied().zip(categories.iter().copied()).collect();
    pairs.sort_unstable();
    let mut out = vec![NO_CATEGORY; groups];
    let mut i = 0;
    while i < pairs.len() {
        let group = pairs[i].0;
        let (mut best, mut most) = (NO_CATEGORY, 0usize);
        while i < pairs.len() && pairs[i].0 == group {
            let category = pairs[i].1;
            let start = i;
            while i < pairs.len() && pairs[i] == (group, category) {
                i += 1;
            }
            if i - start > most {
                (best, most) = (category, i - start);
            }
        }
        out[group as usize] = best;
    }
    out
}

impl View {
    /// Whether a level is drawn in place of the graph.
    pub(super) fn draws_level(&self) -> bool {
        self.level.as_ref().is_some_and(|l| l.scene.is_some())
    }

    /// Make what draws a level, before there is one: its pipelines are
    /// compiled now (and a first, empty-handed draw into a small texture
    /// has a GL driver link them), not when the layout's first level
    /// arrives, which is the frame a large graph is first seen in. For a
    /// graph large enough to be drawn a level at a time; nothing changes
    /// in what is drawn.
    pub fn warm_levels(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) {
        ready(self.warm_levels_paced(device, queue, format, &mut Pace::never()));
    }

    /// [`View::warm_levels`], giving the event loop a turn as `pace` says
    /// between its pieces: the shader, each of the two pipelines, and the
    /// first draw with each, which is when a GL driver links it. On
    /// software WebGL2 they are 10 to 30 ms each, and in one task they
    /// held the page's thread for over 60 ms (#520).
    pub async fn warm_levels_paced(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        pace: &mut Pace,
    ) {
        if self.level.is_some() {
            return;
        }
        let mut gpu = LevelRenderer::new_paced(device, format, pace).await;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("level warm-up"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let disc = crate::levels::Disc {
            x: 0.0,
            y: 0.0,
            radius: 1.0,
            color: [0; 4],
        };
        let link = crate::levels::Link {
            a: [0.0; 2],
            b: [1.0; 2],
            width: 1.0,
            alpha: 0.0,
        };
        let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // A draw for each pipeline, and a turn after each: the driver
        // links one at a time.
        for (discs, links) in [(&[disc][..], &[][..]), (&[][..], &[link][..])] {
            gpu.set(device, queue, discs, links);
            gpu.draw(
                device,
                queue,
                &target,
                &self.camera,
                self.renderer.background,
                self.theme.edge,
            );
            pace.turn().await;
        }
        gpu.set(device, queue, &[], &[]);
        self.level = Some(Level {
            scene: None,
            gpu,
            drew: (0, 0),
            cut: None,
        });
    }

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
        self.put_level(device, format, scene, None);
    }

    fn put_level(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        scene: LevelScene,
        cut: Option<CutGroups>,
    ) {
        self.moved();
        match &mut self.level {
            Some(level) => {
                level.scene = Some(scene);
                level.cut = cut;
            }
            None => {
                self.level = Some(Level {
                    scene: Some(scene),
                    gpu: LevelRenderer::new(device, format),
                    drew: (0, 0),
                    cut,
                })
            }
        }
    }

    /// Draw `scene`, a cut through the layout's hierarchy
    /// ([`crate::cut::Cut`], #498), in place of the graph, until
    /// [`View::clear_level`]: its nodes where the scene has them, each
    /// standing for the `members[i]` nodes `v` of the graph with
    /// `of[v] == i`. Or why that is not a cut of this graph.
    ///
    /// The camera does not move, and goes on fitting the graph if it
    /// follows: the cut is the graph's own drawing, coarser. A disc takes
    /// the colour of most of its nodes ([`View::color_by`]) and a ring in
    /// the selection colour if any of them is selected.
    pub fn set_cut(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        scene: LevelScene,
        members: Vec<u32>,
        of: Vec<u32>,
    ) -> Result<(), String> {
        let groups = scene.n_nodes();
        if members.len() != groups {
            return Err(format!(
                "a cut of {groups} groups has {} member counts",
                members.len()
            ));
        }
        if of.len() != self.n_nodes() {
            return Err(format!(
                "a cut of a graph of {} nodes says which group {} are in",
                self.n_nodes(),
                of.len()
            ));
        }
        if let Some(&group) = of.iter().find(|&&g| g as usize >= groups) {
            return Err(format!(
                "a cut of {groups} groups has a node in group {group}"
            ));
        }
        let category = match &self.categories {
            Some(categories) => majority(&of, groups, categories),
            None => vec![NO_CATEGORY; groups],
        };
        let cut = CutGroups {
            of,
            members,
            category,
        };
        self.put_level(device, format, scene, Some(cut));
        Ok(())
    }

    /// Whether a cut is drawn in place of the graph ([`View::set_cut`]).
    pub fn draws_cut(&self) -> bool {
        self.draws_level() && self.level.as_ref().is_some_and(|l| l.cut.is_some())
    }

    /// How much larger than fitted the level's discs are drawn: by the
    /// square root of how far the view is zoomed in past the fit.
    fn level_zoom(&self) -> f32 {
        (self.camera.pixels_per_unit / self.fitted_zoom)
            .max(1.0)
            .sqrt()
    }

    /// The group whose disc is under the pixel `(px, py)`, if a cut is
    /// drawn and one is: the heaviest of those that are.
    pub fn pick_group(&self, px: f32, py: f32) -> Option<u32> {
        if !self.draws_cut() {
            return None;
        }
        let (w, h) = self.camera.viewport;
        self.level()?.pick(
            self.camera.to_world(px, py),
            self.camera.pixels_per_unit,
            w,
            h,
            self.level_zoom(),
        )
    }

    /// The graph's nodes that group `group` of the cut stands for,
    /// ascending; none if no cut is drawn, or it has no such group.
    pub fn group_nodes(&self, group: u32) -> Vec<u32> {
        let Some(cut) = self.level.as_ref().and_then(|l| l.cut.as_ref()) else {
            return Vec::new();
        };
        (0..cut.of.len() as u32)
            .filter(|&v| cut.of[v as usize] == group)
            .collect()
    }

    /// The group of the cut that node `node` of the graph is in, if a cut
    /// is drawn.
    pub fn group_of(&self, node: u32) -> Option<u32> {
        let cut = self.level.as_ref()?.cut.as_ref()?;
        cut.of.get(node as usize).copied()
    }

    /// The radius group `group` of the cut is drawn with now, in pixels,
    /// if a cut is drawn and has the group.
    pub fn group_radius(&self, group: u32) -> Option<f32> {
        if !self.draws_cut() {
            return None;
        }
        let scene = self.level()?;
        let (w, h) = self.camera.viewport;
        ((group as usize) < scene.n_nodes()).then(|| {
            scene.radius_at(
                group as usize,
                w,
                h,
                self.level_zoom(),
                self.camera.pixels_per_unit,
            )
        })
    }

    /// What is drawn of a cut, if one is.
    pub fn cut_info(&self) -> Option<CutInfo> {
        let level = self.level.as_ref()?;
        let (scene, cut) = (level.scene.as_ref()?, level.cut.as_ref()?);
        Some(CutInfo {
            groups: scene.n_nodes(),
            merged: cut.members.iter().filter(|&&m| m > 1).count(),
            nodes: cut.members.iter().map(|&m| m as usize).sum(),
            groups_drawn: level.drew.0,
            edges: scene.n_edges(),
            edges_drawn: level.drew.1,
            total_edges: scene.total_edges(),
        })
    }

    /// The labels of the cut's groups that are large enough on the canvas
    /// to carry one: the at most `most` heaviest groups of more than one
    /// node whose disc is in view with a radius of at least `min_radius`
    /// pixels, heaviest first. None if no cut is drawn.
    pub fn group_labels(&self, min_radius: f32, most: usize) -> Vec<GroupLabel> {
        let Some(level) = &self.level else {
            return Vec::new();
        };
        let (Some(scene), Some(cut)) = (&level.scene, &level.cut) else {
            return Vec::new();
        };
        let (w, h) = self.camera.viewport;
        let zoom = self.level_zoom();
        let (x, y) = scene.positions();
        let mut labels = Vec::new();
        for &i in scene.by_mass() {
            if labels.len() >= most {
                break;
            }
            let kind = scene.kind(i as usize);
            let radius = scene.radius_at(i as usize, w, h, zoom, self.camera.pixels_per_unit);
            if radius < min_radius {
                // The discs by mass are heaviest first, and so largest
                // first; an open one is as large as it is.
                if kind == crate::levels::Kind::Mass {
                    break;
                }
                continue;
            }
            let members = cut.members[i as usize];
            let (px, py) = self.camera.to_pixel(x[i as usize], y[i as usize]);
            if members > 1 && (0.0..=w).contains(&px) && (0.0..=h).contains(&py) {
                labels.push(GroupLabel {
                    group: i,
                    x: px,
                    y: py,
                    radius,
                    members,
                });
            }
        }
        labels
    }

    /// Put the nodes of the level being drawn where `fill(x, y)` writes
    /// them (one per node each), and fit the camera to them if it follows.
    /// Nothing, if no level is set.
    pub fn set_level_positions(&mut self, fill: impl FnOnce(&mut [f32], &mut [f32])) {
        let Some(scene) = self.level.as_mut().and_then(|l| l.scene.as_mut()) else {
            return;
        };
        let (x, y) = scene.positions_mut();
        fill(x, y);
        self.moved();
        if self.follow {
            self.fit_camera();
        }
    }

    /// Draw the graph again: the level, and what the GPU held of it, are
    /// let go. The graph's nodes are where [`View::set_positions`] put
    /// them, or will.
    ///
    /// After a cut ([`View::set_cut`]) the pipelines stay, for the next
    /// one: a view goes between a cut and the graph as it zooms.
    pub fn clear_level(&mut self) {
        if !self.draws_level() {
            return;
        }
        match &mut self.level {
            Some(level) if level.cut.is_some() => {
                level.scene = None;
                level.cut = None;
                level.drew = (0, 0);
            }
            _ => {
                self.level = None;
                self.fitted = None;
            }
        }
        self.moved();
    }

    /// The level being drawn, if one is.
    pub fn level(&self) -> Option<&LevelScene> {
        self.level.as_ref().and_then(|l| l.scene.as_ref())
    }

    /// What is being drawn of a level, if one is set (not a cut:
    /// [`View::cut_info`]).
    pub fn level_info(&self) -> Option<LevelInfo> {
        let level = self.level.as_ref().filter(|l| l.cut.is_none())?;
        let scene = level.scene.as_ref()?;
        Some(LevelInfo {
            level: scene.level(),
            levels: scene.levels(),
            nodes: scene.n_nodes(),
            nodes_drawn: level.drew.0,
            edges: scene.n_edges(),
            edges_drawn: level.drew.1,
            total_edges: scene.total_edges(),
        })
    }

    /// The box around the level's nodes, if a level is set and has any.
    /// Not a cut's: the camera fits the graph's own nodes under it.
    pub(super) fn level_bounds(&self) -> Option<Option<(f32, f32, f32, f32)>> {
        if self.draws_cut() {
            return None;
        }
        self.level().map(|scene| scene.bounds())
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
        let zoom = self.level_zoom();
        let color = self.theme.node;
        let ppu = self.camera.pixels_per_unit;
        // The groups with a selected node, if a cut is drawn.
        let selected: Option<Vec<bool>> =
            (self.level.as_ref())
                .and_then(|l| l.cut.as_ref())
                .map(|cut| {
                    let mut marked = vec![false; cut.members.len()];
                    for (v, f) in self.highlight.flags().iter().enumerate() {
                        if f & flag::SELECTED != 0 {
                            if let Some(&g) = cut.of.get(v) {
                                marked[g as usize] = true;
                            }
                        }
                    }
                    marked
                });
        let theme = &self.theme;
        if let Some(level) = &mut self.level {
            let Some(scene) = &level.scene else {
                return;
            };
            let (under, d) = match (&level.cut, &selected) {
                (Some(cut), Some(selected)) => {
                    let ink = |i: usize| {
                        (
                            theme.color(cut.category[i]),
                            selected[i].then_some(theme.selection),
                        )
                    };
                    (
                        scene.open_discs(ppu, theme.background, ink),
                        scene.discs_by(discs, w, h, zoom, ink),
                    )
                }
                _ => (Vec::new(), scene.discs(discs, w, h, zoom, color)),
            };
            level.gpu.set_under(device, queue, &under);
            let l = scene.links(links);
            level.gpu.set(device, queue, &d, &l);
            level.drew = (discs.min(scene.n_nodes()), l.len());
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
            .level()
            .map_or((0, 0), |scene| (scene.n_nodes(), scene.n_edges()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_takes_the_category_of_most_of_its_nodes() {
        // Groups 0, 1, 2 and 3 (which has no node).
        let of = [0, 1, 0, 2, 0, 1, 2, 1, 1];
        let categories = [5, 3, 5, 9, 2, 7, 1, 3, NO_CATEGORY];
        // Group 0: two 5s and a 2. Group 1: two 3s, a 7 and one with no
        // category. Group 2: a 9 and a 1, which tie: the lower.
        assert_eq!(majority(&of, 4, &categories), vec![5, 3, 1, NO_CATEGORY]);
        assert_eq!(majority(&[], 2, &[]), vec![NO_CATEGORY; 2]);
        // No category is one too, when most have none.
        assert_eq!(
            majority(&[0, 0, 0], 1, &[NO_CATEGORY, 4, NO_CATEGORY]),
            vec![NO_CATEGORY]
        );
    }
}
