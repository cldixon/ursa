//! Drawing a scene with wgpu: WebGPU or WebGL2 in the browser, Vulkan, Metal
//! or DX12 natively.
//!
//! Two pipelines over the same node positions:
//!
//! - **Edges** are a line list: the edge buffer holds `src, dst` per edge as
//!   indexes into the position buffers, so the edges are uploaded once and a
//!   layout step re-uploads only the positions (8 bytes a node).
//! - **Nodes** are instanced quads, one per node, each shaded as a disc of the
//!   node's radius in pixels (so nodes keep their size as the view zooms).
//!
//! Positions are two vertex buffers, `x` and `y`, exactly as the layout holds
//! them, so an upload copies them without interleaving. Everything is a vertex
//! or index buffer, never a storage buffer, because WebGL2 has none.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::scene::{Camera, NodeStyle};

/// The camera, as the shaders read it.
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
}

const SHADER: &str = r#"
struct Uniforms {
    center: vec2<f32>,
    scale: vec2<f32>,
    pixel: vec2<f32>,
    edge_alpha: f32,
    radius_scale: f32,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

fn clip(x: f32, y: f32) -> vec2<f32> {
    return (vec2<f32>(x, y) - u.center) * u.scale;
}

@vertex
fn edge_vs(@location(0) x: f32, @location(1) y: f32) -> @builtin(position) vec4<f32> {
    return vec4<f32>(clip(x, y), 0.0, 1.0);
}

@fragment
fn edge_fs() -> @location(0) vec4<f32> {
    // Premultiplied: a light grey that darkens where edges pile up.
    let a = u.edge_alpha;
    return vec4<f32>(0.35 * a, 0.35 * a, 0.40 * a, a);
}

struct NodeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius_px: f32,
};

@vertex
fn node_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) x: f32,
    @location(1) y: f32,
    @location(2) radius: f32,
    @location(3) color: vec4<f32>,
) -> NodeOut {
    // Two triangles over the square [-1, 1]^2, padded by a pixel so the
    // edge of the disc can be smoothed.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[corner];
    let scaled = radius * u.radius_scale;
    let r = scaled + 1.0;
    var out: NodeOut;
    out.position = vec4<f32>(clip(x, y) + c * r * u.pixel, 0.0, 1.0);
    out.local = c * r;
    out.color = color;
    out.radius_px = scaled;
    return out;
}

@fragment
fn node_fs(in: NodeOut) -> @location(0) vec4<f32> {
    let d = length(in.local);
    let coverage = clamp(in.radius_px + 0.5 - d, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let a = in.color.a * coverage;
    return vec4<f32>(in.color.rgb * a, a);
}
"#;

/// The GPU side of a scene: its pipelines and buffers.
pub struct Renderer {
    edges: wgpu::RenderPipeline,
    nodes: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    x: wgpu::Buffer,
    y: wgpu::Buffer,
    radius: wgpu::Buffer,
    color: wgpu::Buffer,
    lines: wgpu::Buffer,
    n_nodes: u32,
    n_indices: u32,
    /// The background, white unless set.
    pub background: wgpu::Color,
    /// Edge opacity, from 0 to 1; lower for dense graphs.
    pub edge_alpha: f32,
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
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
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
        // One f32 per vertex (or instance) at each of shader locations 0, 1, 2.
        let float_at = |shader_location| {
            [wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 0,
                shader_location,
            }]
        };
        let floats = [float_at(0), float_at(1), float_at(2)];
        let scalar = |location: usize, step_mode| wgpu::VertexBufferLayout {
            array_stride: 4,
            step_mode,
            attributes: &floats[location],
        };
        let edge_buffers = [
            scalar(0, wgpu::VertexStepMode::Vertex),
            scalar(1, wgpu::VertexStepMode::Vertex),
        ];
        let edges = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("edges"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("edge_vs"),
                compilation_options: Default::default(),
                buffers: &edge_buffers,
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("edge_fs"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            multiview_mask: None,
            cache: None,
        });
        let color_attr = [wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Unorm8x4,
            offset: 0,
            shader_location: 3,
        }];
        let node_buffers = [
            scalar(0, wgpu::VertexStepMode::Instance),
            scalar(1, wgpu::VertexStepMode::Instance),
            scalar(2, wgpu::VertexStepMode::Instance),
            wgpu::VertexBufferLayout {
                array_stride: 4,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &color_attr,
            },
        ];
        let nodes = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nodes"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("node_vs"),
                compilation_options: Default::default(),
                buffers: &node_buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("node_fs"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            multiview_mask: None,
            cache: None,
        });
        let vertex = wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST;
        Self {
            edges,
            nodes,
            uniforms,
            bind_group,
            x: buffer(device, "x", bytemuck::cast_slice(x), vertex),
            y: buffer(device, "y", bytemuck::cast_slice(y), vertex),
            radius: buffer(
                device,
                "radius",
                bytemuck::cast_slice(&style.radius),
                vertex,
            ),
            color: buffer(device, "color", &rgba_bytes(&style.color), vertex),
            lines: buffer(
                device,
                "edges",
                bytemuck::cast_slice(line_indices),
                wgpu::BufferUsages::INDEX,
            ),
            n_nodes: n_nodes as u32,
            n_indices: line_indices.len() as u32,
            background: wgpu::Color::WHITE,
            edge_alpha: default_edge_alpha(n_nodes, line_indices.len() / 2),
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

    /// Upload new radii and colours.
    pub fn set_style(&self, queue: &wgpu::Queue, style: &NodeStyle) {
        if self.n_nodes == 0 {
            return;
        }
        queue.write_buffer(&self.radius, 0, bytemuck::cast_slice(&style.radius));
        queue.write_buffer(&self.color, 0, &rgba_bytes(&style.color));
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
                pass.set_pipeline(&self.edges);
                pass.set_vertex_buffer(0, self.x.slice(..));
                pass.set_vertex_buffer(1, self.y.slice(..));
                pass.set_index_buffer(self.lines.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.n_indices, 0, 0..1);
            }
            if draw_nodes && self.n_nodes > 0 {
                pass.set_pipeline(&self.nodes);
                pass.set_vertex_buffer(0, self.x.slice(..));
                pass.set_vertex_buffer(1, self.y.slice(..));
                pass.set_vertex_buffer(2, self.radius.slice(..));
                pass.set_vertex_buffer(3, self.color.slice(..));
                pass.draw(0..6, 0..self.n_nodes);
            }
        }
        queue.submit([encoder.finish()]);
    }
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
        let module = naga::front::wgsl::parse_str(SHADER).expect("WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("WGSL validates");
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
