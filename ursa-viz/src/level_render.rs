//! Drawing a level of the layout's hierarchy with wgpu ([`crate::levels`]):
//! its merged edges as quads of each edge's own width and opacity, round at
//! the ends, and its nodes over them as discs.
//!
//! A frame of a level is a pass of its own, with its own shader, uniforms
//! and two small instance buffers, rewritten whole when the level's nodes
//! move: nothing of the graph's own buffers ([`crate::render`]) is read or
//! drawn. It is made when the first level is set, and costs a view that
//! never draws one nothing.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use crate::levels::{Disc, Link};
use crate::scene::{rgba, Camera};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    center: [f32; 2],
    /// Clip units per world unit, x and y.
    scale: [f32; 2],
    /// Clip units per pixel, x and y.
    pixel: [f32; 2],
    _pad: [f32; 2],
    /// The edges' ink, straight RGBA.
    edge_color: [f32; 4],
}

const SHADER: &str = r#"
struct Uniforms {
    center: vec2<f32>,
    scale: vec2<f32>,
    pixel: vec2<f32>,
    pad: vec2<f32>,
    edge_color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

fn clip(at: vec2<f32>) -> vec2<f32> {
    return (at - u.center) * u.scale;
}

struct DiscOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius: f32,
};

@vertex
fn disc_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) at: vec2<f32>,
    @location(1) radius: f32,
    @location(2) color: vec4<f32>,
) -> DiscOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[corner];
    // A pixel more than the disc, to smooth its edge in.
    let r = radius + 1.0;
    var out: DiscOut;
    out.position = vec4<f32>(clip(at) + c * r * u.pixel, 0.0, 1.0);
    out.local = c * r;
    out.color = color;
    out.radius = radius;
    return out;
}

@fragment
fn disc_fs(in: DiscOut) -> @location(0) vec4<f32> {
    let coverage = clamp(in.radius + 0.5 - length(in.local), 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let a = in.color.a * coverage;
    return vec4<f32>(in.color.rgb * a, a);
}

struct LinkOut {
    @builtin(position) position: vec4<f32>,
    // Pixels along the segment from its start, and across it.
    @location(0) at: vec2<f32>,
    @location(1) len: f32,
    // Half the width in pixels, and the opacity.
    @location(2) ink: vec2<f32>,
};

@vertex
fn link_vs(
    @builtin(vertex_index) corner: u32,
    @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>,
    @location(2) ink: vec2<f32>,
) -> LinkOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[corner];
    // In pixels from the centre of the view.
    let pa = clip(a) / u.pixel;
    let pb = clip(b) / u.pixel;
    let d = pb - pa;
    let len = length(d);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 1e-6) {
        dir = d / len;
    }
    // Half the width, and a pixel to smooth the edge in.
    let reach = 0.5 * ink.x + 1.0;
    let along = c.x * len + (2.0 * c.x - 1.0) * reach;
    let across = c.y * reach;
    var out: LinkOut;
    out.position = vec4<f32>(
        (pa + dir * along + vec2<f32>(-dir.y, dir.x) * across) * u.pixel, 0.0, 1.0);
    out.at = vec2<f32>(along, across);
    out.len = len;
    out.ink = vec2<f32>(0.5 * ink.x, ink.y);
    return out;
}

@fragment
fn link_fs(in: LinkOut) -> @location(0) vec4<f32> {
    // The distance to the segment.
    let past = max(max(-in.at.x, in.at.x - in.len), 0.0);
    let dist = length(vec2<f32>(past, in.at.y));
    let coverage = clamp(in.ink.x + 0.5 - dist, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let a = u.edge_color.a * in.ink.y * coverage;
    return vec4<f32>(u.edge_color.rgb * a, a);
}
"#;

const DISC_STRIDE: u64 = std::mem::size_of::<Disc>() as u64;
const LINK_STRIDE: u64 = std::mem::size_of::<Link>() as u64;

/// An instance buffer rewritten whole, growing by doubling.
struct Instances {
    label: &'static str,
    buffer: wgpu::Buffer,
    stride: u64,
    capacity: u32,
    len: u32,
}

impl Instances {
    fn new(device: &wgpu::Device, label: &'static str, stride: u64) -> Self {
        let capacity = 64;
        Self {
            label,
            buffer: Self::buffer(device, label, stride, capacity),
            stride,
            capacity,
            len: 0,
        }
    }

    fn buffer(device: &wgpu::Device, label: &str, stride: u64, capacity: u32) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: u64::from(capacity) * stride,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn set<T: Pod>(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, instances: &[T]) {
        debug_assert_eq!(std::mem::size_of::<T>() as u64, self.stride);
        let n = instances.len() as u32;
        if n > self.capacity {
            self.capacity = n.next_power_of_two();
            self.buffer = Self::buffer(device, self.label, self.stride, self.capacity);
        }
        self.len = n;
        if n > 0 {
            queue.write_buffer(&self.buffer, 0, bytemuck::cast_slice(instances));
        }
    }

    fn slice(&self) -> wgpu::BufferSlice<'_> {
        self.buffer.slice(..u64::from(self.len) * self.stride)
    }
}

/// The GPU side of a level: its pipelines and instance buffers.
pub struct LevelRenderer {
    discs: wgpu::RenderPipeline,
    links: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    disc: Instances,
    link: Instances,
}

impl LevelRenderer {
    /// For frames in `format`.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ursa-viz level"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("level uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("level uniforms"),
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
            label: Some("level uniforms"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ursa-viz level"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let target = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let pipeline = |label: &str, vs: &str, fs: &str, buffers: &[wgpu::VertexBufferLayout]| {
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
                    topology: wgpu::PrimitiveTopology::TriangleList,
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
        let attr = |format, offset, shader_location| wgpu::VertexAttribute {
            format,
            offset,
            shader_location,
        };
        let disc_attrs = [
            attr(wgpu::VertexFormat::Float32x2, 0, 0),
            attr(wgpu::VertexFormat::Float32, 8, 1),
            attr(wgpu::VertexFormat::Unorm8x4, 12, 2),
        ];
        let discs = pipeline(
            "level nodes",
            "disc_vs",
            "disc_fs",
            &[wgpu::VertexBufferLayout {
                array_stride: DISC_STRIDE,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &disc_attrs,
            }],
        );
        let link_attrs = [
            attr(wgpu::VertexFormat::Float32x2, 0, 0),
            attr(wgpu::VertexFormat::Float32x2, 8, 1),
            attr(wgpu::VertexFormat::Float32x2, 16, 2),
        ];
        let links = pipeline(
            "level edges",
            "link_vs",
            "link_fs",
            &[wgpu::VertexBufferLayout {
                array_stride: LINK_STRIDE,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &link_attrs,
            }],
        );
        Self {
            discs,
            links,
            uniforms,
            bind_group,
            disc: Instances::new(device, "level nodes", DISC_STRIDE),
            link: Instances::new(device, "level edges", LINK_STRIDE),
        }
    }

    /// Hold exactly these discs and links: what [`LevelRenderer::draw`]
    /// draws, in this order.
    pub fn set(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        discs: &[Disc],
        links: &[Link],
    ) {
        self.disc.set(device, queue, discs);
        self.link.set(device, queue, links);
    }

    /// Draw the level into `target` as `camera` sees it, on `background`:
    /// the links in `edge_color` (`0xRRGGBBAA`), then the discs.
    pub fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        camera: &Camera,
        background: wgpu::Color,
        edge_color: u32,
    ) {
        let (w, h) = camera.viewport;
        let s = camera.pixels_per_unit;
        let u = Uniforms {
            center: [camera.center.0, camera.center.1],
            scale: [2.0 * s / w, 2.0 * s / h],
            pixel: [2.0 / w, 2.0 / h],
            _pad: [0.0; 2],
            edge_color: rgba(edge_color),
        };
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("level frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("level frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(background),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            if self.link.len > 0 {
                pass.set_pipeline(&self.links);
                pass.set_vertex_buffer(0, self.link.slice());
                pass.draw(0..6, 0..self.link.len);
            }
            if self.disc.len > 0 {
                pass.set_pipeline(&self.discs);
                pass.set_vertex_buffer(0, self.disc.slice());
                pass.draw(0..6, 0..self.disc.len);
            }
        }
        queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shader_is_valid_wgsl_and_has_its_entry_points() {
        let module = naga::front::wgsl::parse_str(SHADER).expect("the shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("the shader is valid");
        let names: Vec<&str> = module
            .entry_points
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        for name in ["disc_vs", "disc_fs", "link_vs", "link_fs"] {
            assert!(names.contains(&name), "{name} is missing");
        }
    }

    #[test]
    fn the_uniforms_are_laid_out_as_wgsl_lays_them_out() {
        assert_eq!(std::mem::size_of::<Uniforms>(), 48);
        assert_eq!(std::mem::offset_of!(Uniforms, edge_color), 32);
    }
}
