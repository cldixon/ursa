//! Drawing a scene with wgpu: WebGPU or WebGL2 in the browser, Vulkan, Metal
//! or DX12 natively.
//!
//! Layers, back to front, over the same node positions:
//!
//! - **Edges** are a line list: the edge buffer holds `src, dst` per edge as
//!   indexes into the position buffers, so the edges are uploaded once and a
//!   layout step re-uploads only the positions (8 bytes a node).
//! - **Nodes** are instanced quads, one per node, each shaded as a disc of the
//!   node's radius in pixels (so nodes keep their size as the view zooms),
//!   with a halo or rings from the node's flags ([`crate::highlight::flag`]).
//! - **Highlighted edges** are instanced quads too, one per edge, a width in
//!   pixels wide with round ends, in the theme's highlight colour.
//! - **Nodes in front** (selected, hovered, highlighted, a route's end): the
//!   node pipeline again, for those nodes alone.
//!
//! While a highlight is set, the edges and the nodes not in front draw at
//! the dim level (one uniform): the edges' ink that much fainter, the nodes
//! that far from the background's colour to their own, still opaque.
//!
//! Positions are two vertex buffers, `x` and `y`, exactly as the layout holds
//! them, so an upload copies them without interleaving. Flags are a buffer
//! of their own (four bytes a node, the flags byte first: a vertex buffer's
//! stride is a multiple of four), so a selection uploads a few bytes and
//! never the styles. A highlighted edge is its two ends' positions (16
//! bytes), gathered again when the positions change: an instance cannot
//! index the position buffers, and WebGL2 has no storage buffer or texture
//! fetch cheap enough to stand in. Everything is a vertex or index buffer,
//! never a storage buffer.

use std::borrow::Cow;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::highlight::{DIM_LEVEL, HIGHLIGHT_WIDTH, MARK_REACH, MIN_FRONT_RADIUS};
use crate::scene::{rgba, Camera, NodeStyle, Theme};

/// The camera and the colours, as the shaders read them.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    center: [f32; 2],
    /// Clip units per world unit, x and y.
    scale: [f32; 2],
    /// Clip units per pixel, x and y.
    pixel: [f32; 2],
    edge_alpha: f32,
    /// Multiplies every node's radius (see [`node_radius_scale`]).
    radius_scale: f32,
    /// The edges' ink, straight (not premultiplied) RGBA.
    edge_color: [f32; 4],
    highlight_color: [f32; 4],
    selection_color: [f32; 4],
    pin_color: [f32; 4],
    /// What a dimmed node fades toward.
    background: [f32; 4],
    /// How strongly the edges and the nodes not in front are drawn: 1, or
    /// the dim level while a highlight is set.
    dim_rest: f32,
    /// How strongly a node flagged dimmed is drawn.
    dim_level: f32,
    /// A highlighted edge's width in pixels.
    highlight_width: f32,
    /// The least radius of a node in front, in pixels.
    min_front_radius: f32,
}

/// The flag bits and mark sizes, as WGSL constants: the shader's copy of
/// [`crate::highlight`].
fn shader_source() -> String {
    use crate::highlight::flag;
    let marked = flag::FRONT | flag::PINNED;
    format!(
        "const SELECTED: u32 = {}u;\nconst HOVERED: u32 = {}u;\nconst PINNED: u32 = {}u;\n\
         const DIMMED: u32 = {}u;\nconst HIGHLIGHTED: u32 = {}u;\nconst HIDDEN: u32 = {}u;\n\
         const SEED: u32 = {}u;\nconst FRONT: u32 = {}u;\nconst MARKED: u32 = {}u;\n\
         const MARK_REACH: f32 = {:?};\n{SHADER}",
        flag::SELECTED,
        flag::HOVERED,
        flag::PINNED,
        flag::DIMMED,
        flag::HIGHLIGHTED,
        flag::HIDDEN,
        flag::SEED,
        flag::FRONT,
        marked,
        MARK_REACH,
    )
}

const SHADER: &str = r#"
struct Uniforms {
    center: vec2<f32>,
    scale: vec2<f32>,
    pixel: vec2<f32>,
    edge_alpha: f32,
    radius_scale: f32,
    edge_color: vec4<f32>,
    highlight_color: vec4<f32>,
    selection_color: vec4<f32>,
    pin_color: vec4<f32>,
    background: vec4<f32>,
    dim_rest: f32,
    dim_level: f32,
    highlight_width: f32,
    min_front_radius: f32,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

fn clip(x: f32, y: f32) -> vec2<f32> {
    return (vec2<f32>(x, y) - u.center) * u.scale;
}

// Premultiplied: faint ink that deepens where edges pile up.
fn edge_ink() -> vec4<f32> {
    let a = u.edge_alpha * u.edge_color.a * u.dim_rest;
    return vec4<f32>(u.edge_color.rgb * a, a);
}

@vertex
fn edge_vs(@location(0) x: f32, @location(1) y: f32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(clip(x, y), 0.0, 1.0);
}

@fragment
fn edge_fs() -> @location(0) vec4<f32> {
    return edge_ink();
}

// The edges while a node is hidden: an edge with a hidden end is not drawn.
struct HidingEdgeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) hidden: f32,
};

@vertex
fn hiding_edge_vs(
    @location(0) x: f32,
    @location(1) y: f32,
    @location(2) flags: vec4<u32>,
) -> HidingEdgeOut {
    var out: HidingEdgeOut;
    out.position = vec4<f32>(clip(x, y), 0.0, 1.0);
    out.hidden = f32((flags.x & HIDDEN) != 0u);
    return out;
}

@fragment
fn hiding_edge_fs(in: HidingEdgeOut) -> @location(0) vec4<f32> {
    // 1 at a hidden end, 0 at a shown one: above 0 all along the edge if
    // either end is hidden.
    if (in.hidden > 0.0) {
        discard;
    }
    return edge_ink();
}

struct NodeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius_px: f32,
    // Halo, a route end's ring, the selection ring (1 selected, 0.5
    // hovered), the pin's ring: each 0 without.
    @location(3) marks: vec4<f32>,
    @location(4) fade: f32,
};

fn has(flags: u32, bits: u32) -> f32 {
    return f32((flags & bits) != 0u);
}

// One corner of a node's quad. The nodes in front are drawn in a pass of
// their own (`front_pass`), after the highlighted edges; each pass draws
// nothing for the other's nodes, nor for hidden ones.
fn node(
    corner: u32,
    x: f32,
    y: f32,
    radius: f32,
    color: vec4<f32>,
    flags: u32,
    front_pass: bool,
) -> NodeOut {
    var out: NodeOut;
    let front = (flags & FRONT) != 0u;
    if ((flags & HIDDEN) != 0u || front != front_pass) {
        // Every corner at one point outside the view: no fragments.
        out.position = vec4<f32>(2.0, 2.0, 0.0, 1.0);
        return out;
    }
    // Two triangles over the square [-1, 1]^2, padded by a pixel so the
    // edge of the disc can be smoothed, and by the marks' reach.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[corner];
    var scaled = radius * u.radius_scale;
    if (front) {
        scaled = max(scaled, u.min_front_radius);
    }
    let r = scaled + 1.0 + has(flags, MARKED) * MARK_REACH;
    out.position = vec4<f32>(clip(x, y) + c * r * u.pixel, 0.0, 1.0);
    out.local = c * r;
    out.color = color;
    out.radius_px = scaled;
    out.marks = vec4<f32>(
        has(flags, HIGHLIGHTED),
        has(flags, SEED),
        max(has(flags, SELECTED), 0.5 * has(flags, HOVERED)),
        has(flags, PINNED),
    );
    out.fade = 1.0;
    if (!front) {
        out.fade = mix(u.dim_rest, u.dim_level, has(flags, DIMMED));
    }
    return out;
}

@vertex
fn node_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) x: f32,
    @location(1) y: f32,
    @location(2) radius: f32,
    @location(3) color: vec4<f32>,
    @location(4) flags: vec4<u32>,
) -> NodeOut {
    return node(corner, x, y, radius, color, flags.x, false);
}

@vertex
fn front_node_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) x: f32,
    @location(1) y: f32,
    @location(2) radius: f32,
    @location(3) color: vec4<f32>,
    @location(4) flags: vec4<u32>,
) -> NodeOut {
    return node(corner, x, y, radius, color, flags.x, true);
}

// How much of a pixel `d` from the centre a ring from `lo` to `hi` covers.
fn ring(d: f32, lo: f32, hi: f32) -> f32 {
    return clamp(min(d - lo, hi - d) + 0.5, 0.0, 1.0);
}

// `color` at `coverage` over `below` (premultiplied).
fn over(below: vec4<f32>, color: vec4<f32>, coverage: f32) -> vec4<f32> {
    let a = color.a * coverage;
    return vec4<f32>(color.rgb * a, a) + below * (1.0 - a);
}

@fragment
fn node_fs(in: NodeOut) -> @location(0) vec4<f32> {
    let d = length(in.local);
    let r = in.radius_px;
    var c = vec4<f32>(0.0);
    // The halo: a glow fading out past the disc.
    let glow = 0.5 * clamp(1.0 - (d - r) / (MARK_REACH - 2.0), 0.0, 1.0);
    c = over(c, u.highlight_color, in.marks.x * glow);
    // The disc.
    c = over(c, in.color, clamp(r + 0.5 - d, 0.0, 1.0));
    // The pin: a small ring hugging the disc.
    c = over(c, u.pin_color, in.marks.w * ring(d, r + 0.5, r + 2.0));
    // A route's end, or a seed.
    c = over(c, u.highlight_color, in.marks.y * ring(d, r + 3.0, r + 5.0));
    // Selected (2 px) or hovered (1 px), outside a route end's ring.
    let s = r + 3.0 + 3.0 * in.marks.y;
    let width = 1.0 + step(0.75, in.marks.z);
    c = over(c, u.selection_color, step(0.25, in.marks.z) * ring(d, s, s + width));
    if (c.a <= 0.0) {
        discard;
    }
    // Dimmed, a node fades toward the background but stays opaque: the
    // edges under it do not show through.
    return vec4<f32>(mix(u.background.rgb * c.a, c.rgb, in.fade), c.a);
}

// A highlighted edge: a quad along the segment, round at both ends.
struct WideOut {
    @builtin(position) position: vec4<f32>,
    // Pixels along the segment from its start, and across it.
    @location(0) at: vec2<f32>,
    @location(1) len: f32,
};

@vertex
fn wide_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>,
) -> WideOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[corner];
    // In pixels from the centre of the view.
    let pa = clip(a.x, a.y) / u.pixel;
    let pb = clip(b.x, b.y) / u.pixel;
    let d = pb - pa;
    let len = length(d);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 1e-6) {
        dir = d / len;
    }
    // Half the width, and a pixel to smooth the edge in.
    let reach = 0.5 * u.highlight_width + 1.0;
    let along = c.x * len + (2.0 * c.x - 1.0) * reach;
    let across = c.y * reach;
    var out: WideOut;
    out.position = vec4<f32>(
        (pa + dir * along + vec2<f32>(-dir.y, dir.x) * across) * u.pixel, 0.0, 1.0);
    out.at = vec2<f32>(along, across);
    out.len = len;
    return out;
}

@fragment
fn wide_fs(in: WideOut) -> @location(0) vec4<f32> {
    // The distance to the segment.
    let past = max(max(-in.at.x, in.at.x - in.len), 0.0);
    let dist = length(vec2<f32>(past, in.at.y));
    let coverage = clamp(0.5 * u.highlight_width + 0.5 - dist, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let a = u.highlight_color.a * coverage;
    return vec4<f32>(u.highlight_color.rgb * a, a);
}
"#;

/// The GPU side of a scene: its pipelines and buffers.
pub struct Renderer {
    edges: wgpu::RenderPipeline,
    /// The edges while a node is hidden: it reads the flags too.
    hiding_edges: wgpu::RenderPipeline,
    nodes: wgpu::RenderPipeline,
    front_nodes: wgpu::RenderPipeline,
    wide_edges: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    x: wgpu::Buffer,
    y: wgpu::Buffer,
    radius: wgpu::Buffer,
    color: wgpu::Buffer,
    /// Four bytes a node: its flags, then three unused.
    flags: wgpu::Buffer,
    lines: wgpu::Buffer,
    /// The highlighted edges: each its two ends' positions.
    wide: wgpu::Buffer,
    /// How many highlighted edges `wide` has room for, and how many it holds.
    wide_capacity: u32,
    n_wide: u32,
    n_nodes: u32,
    n_indices: u32,
    /// What the flags ask of a frame ([`Renderer::set_flags`]).
    dims_the_rest: bool,
    any_front: bool,
    any_hidden: bool,
    /// Bytes of flags and of highlighted edges written to the GPU so far.
    flag_bytes_uploaded: u64,
    highlight_bytes_uploaded: u64,
    /// The background, white unless set.
    pub background: wgpu::Color,
    /// The edges' ink, `0xRRGGBBAA` (grey unless set; see [`crate::scene::Theme`]).
    pub edge_color: u32,
    /// The colours of highlighted edges, halos and a route's ends; of the
    /// selection's rings; and of the pins', each `0xRRGGBBAA`.
    pub highlight_color: u32,
    pub selection_color: u32,
    pub pin_color: u32,
    /// Edge opacity, from 0 to 1; lower for dense graphs.
    pub edge_alpha: f32,
    /// How strongly the rest is drawn while a highlight is set, from 0 to 1.
    pub dim_level: f32,
    /// A highlighted edge's width in pixels.
    pub highlight_width: f32,
}

/// How much to shrink every node so that, fitted to a `width` x `height`
/// canvas, the nodes cover at most `coverage` of it: 1 for a small graph,
/// down to `0.15` for a huge one (131K nodes on a laptop screen).
pub fn node_radius_scale(radius: &[f32], width: f32, height: f32, coverage: f32) -> f32 {
    let area: f32 = radius.iter().map(|r| std::f32::consts::PI * r * r).sum();
    if area <= 0.0 {
        return 1.0;
    }
    (coverage * width * height / area).sqrt().clamp(0.15, 1.0)
}

/// Opacity for `n_edges` edges over `n_nodes` nodes: lighter as edges pile up,
/// so a dense graph reads as density rather than a solid blot.
pub fn default_edge_alpha(n_nodes: usize, n_edges: usize) -> f32 {
    let per_node = n_edges as f32 / n_nodes.max(1) as f32;
    (0.6 / (1.0 + per_node * (n_nodes as f32).log10().max(1.0) / 20.0)).clamp(0.03, 0.6)
}

fn buffer(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    // A zero-sized buffer cannot be bound; keep four bytes for an empty graph.
    let contents: &[u8] = if contents.is_empty() {
        &[0; 4]
    } else {
        contents
    };
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage,
    })
}

/// A buffer for `capacity` highlighted edges, 16 bytes each.
fn wide_buffer(device: &wgpu::Device, capacity: u32) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("highlighted edges"),
        size: u64::from(capacity) * WIDE_STRIDE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// A highlighted edge on the GPU: `x, y` of each end.
const WIDE_STRIDE: u64 = 16;

impl Renderer {
    /// Pipelines for drawing into textures of `format`, and buffers for a
    /// graph whose edges are `line_indices` (`src, dst` per edge).
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        x: &[f32],
        y: &[f32],
        style: &NodeStyle,
        line_indices: &[u32],
    ) -> Self {
        let n_nodes = x.len();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ursa-viz"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(shader_source())),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniforms"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ursa-viz"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let blend = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let target = [Some(wgpu::ColorTargetState {
            format,
            blend,
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let pipeline = |label: &str,
                        vs: &str,
                        fs: &str,
                        topology: wgpu::PrimitiveTopology,
                        buffers: &[wgpu::VertexBufferLayout]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &target,
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        // One attribute per buffer, four bytes a vertex (or an instance).
        let attribute = |format, shader_location| {
            [wgpu::VertexAttribute {
                format,
                offset: 0,
                shader_location,
            }]
        };
        let float = wgpu::VertexFormat::Float32;
        let floats = [
            attribute(float, 0),
            attribute(float, 1),
            attribute(float, 2),
        ];
        let color_attr = attribute(wgpu::VertexFormat::Unorm8x4, 3);
        // The flags, where each pipeline reads them.
        let edge_flags = attribute(wgpu::VertexFormat::Uint8x4, 2);
        let node_flags = attribute(wgpu::VertexFormat::Uint8x4, 4);
        let per = |attributes, step_mode| wgpu::VertexBufferLayout {
            array_stride: 4,
            step_mode,
            attributes,
        };
        let (vertex, instance) = (wgpu::VertexStepMode::Vertex, wgpu::VertexStepMode::Instance);
        let lines = wgpu::PrimitiveTopology::LineList;
        let triangles = wgpu::PrimitiveTopology::TriangleList;
        let edge_buffers = [per(&floats[0], vertex), per(&floats[1], vertex)];
        let edges = pipeline("edges", "edge_vs", "edge_fs", lines, &edge_buffers);
        let hiding_edge_buffers = [
            per(&floats[0], vertex),
            per(&floats[1], vertex),
            per(&edge_flags, vertex),
        ];
        let hiding_edges = pipeline(
            "edges, some hidden",
            "hiding_edge_vs",
            "hiding_edge_fs",
            lines,
            &hiding_edge_buffers,
        );
        let node_buffers = [
            per(&floats[0], instance),
            per(&floats[1], instance),
            per(&floats[2], instance),
            per(&color_attr, instance),
            per(&node_flags, instance),
        ];
        let nodes = pipeline("nodes", "node_vs", "node_fs", triangles, &node_buffers);
        let front_nodes = pipeline(
            "nodes in front",
            "front_node_vs",
            "node_fs",
            triangles,
            &node_buffers,
        );
        let wide_attrs = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
        ];
        let wide_buffers = [wgpu::VertexBufferLayout {
            array_stride: WIDE_STRIDE,
            step_mode: instance,
            attributes: &wide_attrs,
        }];
        let wide_edges = pipeline(
            "highlighted edges",
            "wide_vs",
            "wide_fs",
            triangles,
            &wide_buffers,
        );
        let usage = wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST;
        let theme = Theme::light();
        let wide_capacity = 16;
        Self {
            edges,
            hiding_edges,
            nodes,
            front_nodes,
            wide_edges,
            uniforms,
            bind_group,
            x: buffer(device, "x", bytemuck::cast_slice(x), usage),
            y: buffer(device, "y", bytemuck::cast_slice(y), usage),
            radius: buffer(device, "radius", bytemuck::cast_slice(&style.radius), usage),
            color: buffer(device, "color", &rgba_bytes(&style.color), usage),
            flags: buffer(device, "flags", &vec![0; 4 * n_nodes], usage),
            lines: buffer(
                device,
                "edges",
                bytemuck::cast_slice(line_indices),
                wgpu::BufferUsages::INDEX,
            ),
            wide: wide_buffer(device, wide_capacity),
            wide_capacity,
            n_wide: 0,
            n_nodes: n_nodes as u32,
            n_indices: line_indices.len() as u32,
            dims_the_rest: false,
            any_front: false,
            any_hidden: false,
            flag_bytes_uploaded: 0,
            highlight_bytes_uploaded: 0,
            background: wgpu::Color::WHITE,
            edge_color: theme.edge,
            highlight_color: theme.highlight,
            selection_color: theme.selection,
            pin_color: theme.pin,
            edge_alpha: default_edge_alpha(n_nodes, line_indices.len() / 2),
            dim_level: DIM_LEVEL,
            highlight_width: HIGHLIGHT_WIDTH,
        }
    }

    /// Upload new positions (one per node, as when the renderer was made).
    pub fn set_positions(&self, queue: &wgpu::Queue, x: &[f32], y: &[f32]) {
        if self.n_nodes == 0 {
            return;
        }
        queue.write_buffer(&self.x, 0, bytemuck::cast_slice(x));
        queue.write_buffer(&self.y, 0, bytemuck::cast_slice(y));
    }

    /// Draw with `theme`'s background, edge ink and highlight colours (node
    /// colours are the style's: see [`crate::view::View::set_theme`]).
    pub fn set_theme(&mut self, theme: &Theme) {
        let [r, g, b, a] = rgba(theme.background);
        self.background = wgpu::Color {
            r: r.into(),
            g: g.into(),
            b: b.into(),
            a: a.into(),
        };
        self.edge_color = theme.edge;
        self.highlight_color = theme.highlight;
        self.selection_color = theme.selection;
        self.pin_color = theme.pin;
    }

    /// Upload new radii and colours.
    pub fn set_style(&self, queue: &wgpu::Queue, style: &NodeStyle) {
        if self.n_nodes == 0 {
            return;
        }
        queue.write_buffer(&self.radius, 0, bytemuck::cast_slice(&style.radius));
        queue.write_buffer(&self.color, 0, &rgba_bytes(&style.color));
    }

    /// Upload the flags of the nodes in `dirty` (see
    /// [`HighlightState::take_dirty`]), four bytes a node, and note what
    /// `highlight` asks of a frame: whether the rest is dimmed, whether any
    /// node is in front, whether any is hidden. Nothing else is uploaded.
    ///
    /// [`HighlightState::take_dirty`]: crate::highlight::HighlightState::take_dirty
    pub fn set_flags(
        &mut self,
        queue: &wgpu::Queue,
        highlight: &crate::highlight::HighlightState,
        dirty: &[Range<usize>],
    ) {
        let flags = highlight.flags();
        for run in dirty {
            let bytes = flag_bytes(&flags[run.clone()]);
            queue.write_buffer(&self.flags, 4 * run.start as u64, &bytes);
            self.flag_bytes_uploaded += bytes.len() as u64;
        }
        self.dims_the_rest = highlight.dims_the_rest();
        self.any_front = highlight.any_front();
        self.any_hidden = highlight.any_hidden();
    }

    /// Draw these highlighted edges: each `[x, y, x, y]`, its two ends. The
    /// buffer grows by doubling; the graph's own edges are never touched.
    pub fn set_highlight_edges(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ends: &[[f32; 4]],
    ) {
        let n = ends.len() as u32;
        if n > self.wide_capacity {
            self.wide_capacity = n.next_power_of_two();
            self.wide = wide_buffer(device, self.wide_capacity);
        }
        self.n_wide = n;
        self.write_highlight_edges(queue, ends);
    }

    /// The same highlighted edges at new positions: as many as were last
    /// set ([`Renderer::set_highlight_edges`]).
    pub fn write_highlight_edges(&mut self, queue: &wgpu::Queue, ends: &[[f32; 4]]) {
        assert_eq!(ends.len() as u32, self.n_wide, "set the edges first");
        if ends.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(ends);
        queue.write_buffer(&self.wide, 0, bytes);
        self.highlight_bytes_uploaded += bytes.len() as u64;
    }

    /// Bytes of flags written to the GPU so far.
    pub fn flag_bytes_uploaded(&self) -> u64 {
        self.flag_bytes_uploaded
    }

    /// Bytes of highlighted edges written to the GPU so far.
    pub fn highlight_bytes_uploaded(&self) -> u64 {
        self.highlight_bytes_uploaded
    }

    /// Draw the scene into `target` as `camera` sees it.
    pub fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        camera: &Camera,
        draw_nodes: bool,
        radius_scale: f32,
    ) {
        let (w, h) = camera.viewport;
        let s = camera.pixels_per_unit;
        let u = Uniforms {
            center: [camera.center.0, camera.center.1],
            scale: [2.0 * s / w, 2.0 * s / h],
            pixel: [2.0 / w, 2.0 / h],
            edge_alpha: self.edge_alpha,
            radius_scale,
            edge_color: rgba(self.edge_color),
            highlight_color: rgba(self.highlight_color),
            selection_color: rgba(self.selection_color),
            pin_color: rgba(self.pin_color),
            background: [
                self.background.r as f32,
                self.background.g as f32,
                self.background.b as f32,
                self.background.a as f32,
            ],
            dim_rest: if self.dims_the_rest {
                self.dim_level
            } else {
                1.0
            },
            dim_level: self.dim_level,
            highlight_width: self.highlight_width,
            min_front_radius: MIN_FRONT_RADIUS,
        };
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.background),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            if self.n_indices > 0 {
                // Reading the flags costs a little on every edge: only
                // while a node is hidden.
                if self.any_hidden {
                    pass.set_pipeline(&self.hiding_edges);
                    pass.set_vertex_buffer(2, self.flags.slice(..));
                } else {
                    pass.set_pipeline(&self.edges);
                }
                pass.set_vertex_buffer(0, self.x.slice(..));
                pass.set_vertex_buffer(1, self.y.slice(..));
                pass.set_index_buffer(self.lines.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.n_indices, 0, 0..1);
            }
            let draw_nodes = draw_nodes && self.n_nodes > 0;
            let bind_nodes = |pass: &mut wgpu::RenderPass| {
                pass.set_vertex_buffer(0, self.x.slice(..));
                pass.set_vertex_buffer(1, self.y.slice(..));
                pass.set_vertex_buffer(2, self.radius.slice(..));
                pass.set_vertex_buffer(3, self.color.slice(..));
                pass.set_vertex_buffer(4, self.flags.slice(..));
            };
            if draw_nodes {
                pass.set_pipeline(&self.nodes);
                bind_nodes(&mut pass);
                pass.draw(0..6, 0..self.n_nodes);
            }
            if self.n_wide > 0 {
                pass.set_pipeline(&self.wide_edges);
                pass.set_vertex_buffer(0, self.wide.slice(..u64::from(self.n_wide) * WIDE_STRIDE));
                pass.draw(0..6, 0..self.n_wide);
            }
            if draw_nodes && self.any_front {
                pass.set_pipeline(&self.front_nodes);
                bind_nodes(&mut pass);
                pass.draw(0..6, 0..self.n_nodes);
            }
        }
        queue.submit([encoder.finish()]);
    }
}

/// Flags as the GPU holds them: four bytes a node, the flags byte first.
fn flag_bytes(flags: &[u8]) -> Vec<u8> {
    flags.iter().flat_map(|&f| [f, 0, 0, 0]).collect()
}

/// `0xRRGGBBAA` colours as the bytes `Unorm8x4` reads: R, G, B, A.
fn rgba_bytes(colors: &[u32]) -> Vec<u8> {
    colors.iter().flat_map(|c| c.to_be_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(&shader_source()).expect("WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("WGSL validates");
    }

    #[test]
    fn the_uniforms_are_laid_out_as_wgsl_lays_them_out() {
        // vec4 aligns to 16 bytes: edge_color follows radius_scale at 32,
        // and the four scalars after the colours fill the last 16.
        assert_eq!(std::mem::size_of::<Uniforms>(), 128);
        assert_eq!(std::mem::offset_of!(Uniforms, edge_color), 32);
        assert_eq!(std::mem::offset_of!(Uniforms, highlight_color), 48);
        assert_eq!(std::mem::offset_of!(Uniforms, selection_color), 64);
        assert_eq!(std::mem::offset_of!(Uniforms, pin_color), 80);
        assert_eq!(std::mem::offset_of!(Uniforms, background), 96);
        assert_eq!(std::mem::offset_of!(Uniforms, dim_rest), 112);
        assert_eq!(std::mem::offset_of!(Uniforms, min_front_radius), 124);
    }

    #[test]
    fn the_shader_has_every_entry_point_the_pipelines_name() {
        let module = naga::front::wgsl::parse_str(&shader_source()).expect("WGSL parses");
        let names: Vec<&str> = module
            .entry_points
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        for name in [
            "edge_vs",
            "edge_fs",
            "hiding_edge_vs",
            "hiding_edge_fs",
            "node_vs",
            "front_node_vs",
            "node_fs",
            "wide_vs",
            "wide_fs",
        ] {
            assert!(names.contains(&name), "{name} is missing from {names:?}");
        }
    }

    #[test]
    fn flags_are_uploaded_four_bytes_a_node() {
        assert_eq!(flag_bytes(&[1, 0x60]), vec![1, 0, 0, 0, 0x60, 0, 0, 0]);
    }

    #[test]
    fn colours_are_uploaded_as_rgba_bytes() {
        assert_eq!(rgba_bytes(&[0x11223344]), vec![0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn nodes_shrink_only_when_they_would_cover_the_canvas() {
        assert_eq!(node_radius_scale(&[3.0; 34], 800.0, 600.0, 0.08), 1.0);
        let crowded = node_radius_scale(&vec![1.5; 131_072], 1000.0, 700.0, 0.08);
        assert!((0.15..0.5).contains(&crowded), "{crowded}");
        assert_eq!(node_radius_scale(&[], 800.0, 600.0, 0.08), 1.0);
    }

    #[test]
    fn edges_get_lighter_as_they_pile_up() {
        let sparse = default_edge_alpha(34, 78);
        let dense = default_edge_alpha(131_072, 1_000_000);
        assert!(sparse > dense, "{sparse} > {dense}");
        assert!((0.03..=0.6).contains(&dense));
    }
}
