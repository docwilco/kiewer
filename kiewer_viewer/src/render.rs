//! Drawing with wgpu at the WebGL2 floor (ADR 0001): one pipeline over
//! flat triangles, a vertex buffer and an index buffer, one uniform for
//! the camera and one for the colour of each draw, blended over what is
//! already there so a layer can be translucent; beside it the text pass
//! of [`atlas_pipeline`], textured quads over a glyph atlas per face. No
//! storage buffers, no compute, so the same code runs on WebGPU, on
//! WebGL2 and natively.
//!
//! A frame is a list of [`Draw`]s, each a run of the index buffer in one
//! colour, drawn back to front in the order given: the scene decides what
//! that order is and which runs are in it, this module only draws them.
//!
//! Nothing here knows about a canvas or a window: the surface and the
//! adapter are the platform's (`web.rs` in the browser); this module takes
//! a device, a format and a texture view to draw into.

pub mod atlas_pipeline;
#[cfg(not(target_arch = "wasm32"))]
pub mod offscreen;

use std::num::NonZeroU64;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use derive_more::Display;
use wgpu::util::DeviceExt as _;

use crate::atlas::TextVertex;
use crate::camera::View;
use crate::render::atlas_pipeline::{Coverage, TextPass};

/// The shader source, compiled at pipeline creation.
pub const SHADER: &str = include_str!("shader.wgsl");

/// The limits every device is asked for: WebGL2's, so what runs on
/// WebGPU is what runs on WebGL2. They have no storage buffers and no
/// compute.
#[must_use]
pub fn limits() -> wgpu::Limits {
    wgpu::Limits::downlevel_webgl2_defaults()
}

/// The device every platform requests: no optional features, the
/// [`limits`] above.
#[must_use]
pub fn device_descriptor() -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("kiewer viewer"),
        required_features: wgpu::Features::empty(),
        required_limits: limits(),
        ..Default::default()
    }
}

/// Which API a viewer draws with, for the page and the beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
pub enum Backend {
    #[display("webgpu")]
    WebGpu,
    #[display("webgl2")]
    WebGl2,
}

/// Samples per pixel when the target format allows it: the edges of
/// tracks and pads without a shimmer, at the cost of a four times larger
/// render target.
pub const MSAA_SAMPLES: u32 = 4;

/// One vertex: a position in millimetres relative to the scene's origin.
/// Its colour is the draw's ([`Draw::color`]), not its own.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
}

impl Vertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
    };
}

/// One draw of a frame: a run of the index buffer in one colour, the
/// layer's opacity as its alpha, as the target format takes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Draw {
    /// The indices, a multiple of three.
    pub range: Range<u32>,
    /// Red, green, blue as the shader writes them, and alpha.
    pub color: [f32; 4],
}

/// Where a frame goes: a texture of the renderer's format, its size in
/// pixels, and the colour it is cleared to first.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub view: &'a wgpu::TextureView,
    pub size: (u32, u32),
    pub clear: wgpu::Color,
}

/// The colour uniform of one draw, as the shader reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct Paint {
    color: [f32; 4],
}

/// The step between one draw's paint and the next in the one uniform
/// buffer that holds them all: the offset alignment WebGL2 requires,
/// which the limits name as 256.
const PAINT_STRIDE: usize = 256;

/// A colour as the page names it (`#2b2b2b`), converted for the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The colour as the shader writes it into a target of `format`: for
    /// an sRGB format the hardware encodes, so the shader writes linear
    /// light; for a plain format the shader's value is shown as it is, so
    /// it stays sRGB-encoded.
    #[must_use]
    pub fn for_format(self, format: wgpu::TextureFormat) -> [f32; 4] {
        let channel = |byte: u8| {
            let encoded = f32::from(byte) / 255.0;
            if format.is_srgb() {
                srgb_to_linear(encoded)
            } else {
                encoded
            }
        };
        [channel(self.0), channel(self.1), channel(self.2), 1.0]
    }

    /// The colour as CSS writes it: `#f0c030`.
    #[must_use]
    pub fn to_css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// The colour as a clear value for a target of `format`.
    #[must_use]
    pub fn clear_for_format(self, format: wgpu::TextureFormat) -> wgpu::Color {
        let [r, g, b, a] = self.for_format(format);
        wgpu::Color {
            r: f64::from(r),
            g: f64::from(g),
            b: f64::from(b),
            a: f64::from(a),
        }
    }
}

/// The sRGB transfer function, decoding.
fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// The pipeline and the buffers over one device, drawing into targets of
/// one format.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    paint_layout: wgpu::BindGroupLayout,
    paints: Option<Paints>,
    geometry: Option<Geometry>,
    format: wgpu::TextureFormat,
    sample_count: u32,
    msaa: Option<MsaaTarget>,
    /// Named-face text through the glyph atlas (`atlas_pipeline`), drawn
    /// after the flat geometry.
    text: TextPass,
}

struct Geometry {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// The paints of a frame's draws, one [`PAINT_STRIDE`] apart in one
/// buffer, bound once with a dynamic offset per draw; regrown when a
/// frame has more draws than it holds.
struct Paints {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    capacity: usize,
}

struct MsaaTarget {
    size: (u32, u32),
    view: wgpu::TextureView,
}

impl Renderer {
    /// A renderer on `device` for targets of `format`, sampling
    /// `sample_count` times per pixel (1, or [`MSAA_SAMPLES`] when the
    /// adapter reports the format supports it).
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, sample_count: u32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kiewer viewer shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("view"),
            contents: bytemuck::bytes_of(&View {
                center: [0.0, 0.0],
                scale: [1.0, 1.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let paint_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("paint"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new(std::mem::size_of::<Paint>() as u64),
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kiewer viewer"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(&paint_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("flat triangles"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(Vertex::LAYOUT)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Straight alpha over what is there: a layer at an
                    // opacity shows the layers under it.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Both windings: holes are wound against their copper
                // (kicad_core::tessellate) and the triangles come out as
                // lyon lays them.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        });
        let text = TextPass::new(device, format, sample_count, &bind_group_layout);
        Self {
            pipeline,
            uniform,
            bind_group,
            paint_layout,
            paints: None,
            geometry: None,
            format,
            sample_count,
            msaa: None,
            text,
        }
    }

    /// Replace the glyph atlas and the quads of one face of text
    /// (`crate::atlas`); no indices drops the face.
    pub fn upload_text(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        face: &str,
        coverage: &Coverage<'_>,
        vertices: &[TextVertex],
        indices: &[u32],
    ) {
        self.text
            .upload(device, queue, face, coverage, vertices, indices);
    }

    /// Drop every face of text: a new board, or a face sent back to the
    /// stroke font.
    pub fn clear_text(&mut self) {
        self.text.clear();
    }

    /// Drop one face of text.
    pub fn remove_text(&mut self, face: &str) {
        self.text.remove(face);
    }

    /// Samples per pixel the frames are drawn with.
    #[must_use]
    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    /// Replace the geometry: `vertices` and `indices` in threes.
    pub fn upload(&mut self, device: &wgpu::Device, vertices: &[Vertex], indices: &[u32]) {
        if indices.is_empty() {
            self.geometry = None;
            return;
        }
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertices"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("indices"),
            contents: bytemuck::cast_slice(indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        self.geometry = Some(Geometry {
            vertices: vertex_buffer,
            indices: index_buffer,
            index_count: u32::try_from(indices.len()).unwrap_or(u32::MAX),
        });
    }

    /// Draw `draws`, in order, as `view` sees them into `target`, over
    /// its clear colour, and the named-face text over them when
    /// `named_text` (its layer is shown). A draw whose range runs past
    /// the geometry is skipped.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &Target<'_>,
        view: View,
        draws: &[Draw],
        named_text: bool,
    ) {
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&view));
        self.grow_paints(device, draws.len());
        let msaa_view = (self.sample_count > 1).then(|| self.msaa_view(device, target.size));
        let (attachment, resolve_target) = match &msaa_view {
            Some(msaa) => (msaa, Some(target.view)),
            None => (target.view, None),
        };
        let Some(paints) = &self.paints else {
            // grow_paints made them; nothing to draw into otherwise.
            return;
        };
        let clear = target.clear;
        // Every draw's paint in one write, each at its stride.
        if !draws.is_empty() {
            let mut packed = vec![0u8; draws.len() * PAINT_STRIDE];
            for (chunk, draw) in packed
                .as_chunks_mut::<PAINT_STRIDE>()
                .0
                .iter_mut()
                .zip(draws)
            {
                let paint = Paint { color: draw.color };
                let bytes = bytemuck::bytes_of(&paint);
                if let Some(slot) = chunk.get_mut(..bytes.len()) {
                    slot.copy_from_slice(bytes);
                }
            }
            queue.write_buffer(&paints.buffer, 0, &packed);
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("board"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: attachment,
                    depth_slice: None,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: if resolve_target.is_some() {
                            wgpu::StoreOp::Discard
                        } else {
                            wgpu::StoreOp::Store
                        },
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(geometry) = &self.geometry {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, geometry.vertices.slice(..));
                pass.set_index_buffer(geometry.indices.slice(..), wgpu::IndexFormat::Uint32);
                for (index, draw) in draws.iter().enumerate() {
                    if draw.range.end > geometry.index_count || draw.range.is_empty() {
                        continue;
                    }
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a frame has tens of draws, so the offset is far below 2^32"
                    )]
                    let offset = (index * PAINT_STRIDE) as u32;
                    pass.set_bind_group(1, &paints.bind_group, &[offset]);
                    pass.draw_indexed(draw.range.clone(), 0, 0..1);
                }
            }
            // Named-face text over every layer when its layer is shown
            // (#46 puts it in its layer's slot). The view is bound again:
            // the slices above bind nothing at group 0 today, and the
            // text pass relies on it whatever they come to do.
            if named_text {
                pass.set_bind_group(0, &self.bind_group, &[]);
                self.text.draw(&mut pass);
            }
        }
        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Make sure the paint buffer has room for `count` draws, remaking it
    /// larger when it has not.
    fn grow_paints(&mut self, device: &wgpu::Device, count: usize) {
        let count = count.max(1);
        if self
            .paints
            .as_ref()
            .is_none_or(|paints| paints.capacity < count)
        {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("paints"),
                size: (count * PAINT_STRIDE) as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("paint"),
                layout: &self.paint_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: NonZeroU64::new(std::mem::size_of::<Paint>() as u64),
                    }),
                }],
            });
            self.paints = Some(Paints {
                buffer,
                bind_group,
                capacity: count,
            });
        }
    }

    /// The multisampled texture the frame is drawn into before the
    /// resolve, remade when the target's size changes.
    fn msaa_view(&mut self, device: &wgpu::Device, size: (u32, u32)) -> wgpu::TextureView {
        if let Some(msaa) = &self.msaa
            && msaa.size == size
        {
            return msaa.view.clone();
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("msaa"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: self.sample_count,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.msaa = Some(MsaaTarget {
            size,
            view: view.clone(),
        });
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_is_asked_for_the_webgl2_limits_with_no_storage_or_compute() {
        let descriptor = device_descriptor();
        assert_eq!(descriptor.required_features, wgpu::Features::empty());
        let limits = &descriptor.required_limits;
        assert_eq!(limits, &wgpu::Limits::downlevel_webgl2_defaults());
        assert_eq!(limits.max_storage_buffers_per_shader_stage, 0);
        assert_eq!(limits.max_storage_textures_per_shader_stage, 0);
        assert_eq!(limits.max_dynamic_storage_buffers_per_pipeline_layout, 0);
        assert_eq!(limits.max_compute_workgroup_size_x, 0);
        assert_eq!(limits.max_compute_workgroups_per_dimension, 0);
        assert_eq!(limits.max_compute_invocations_per_workgroup, 0);
        // The paints are one dynamic-offset uniform at the alignment the
        // limits require; the two bind groups fit the floor.
        assert!(limits.max_dynamic_uniform_buffers_per_pipeline_layout >= 1);
        assert!(limits.max_bind_groups >= 2);
        assert_eq!(
            PAINT_STRIDE
                % usize::try_from(limits.min_uniform_buffer_offset_alignment).expect("fits"),
            0
        );
        assert!(PAINT_STRIDE >= std::mem::size_of::<Paint>());
    }

    #[test]
    fn the_shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(SHADER).expect("the shader parses");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        validator
            .validate(&module)
            .expect("the shader validates with no capabilities beyond the baseline");
        let entry_points: Vec<&str> = module
            .entry_points
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(entry_points, ["vs_main", "fs_main"]);
        // The vertex stage takes the one input struct the layout fills.
        let vertex = module
            .entry_points
            .iter()
            .find(|entry| entry.name == "vs_main")
            .expect("vs_main");
        assert_eq!(vertex.function.arguments.len(), 1);
        // The two uniforms, the view at group 0 and the paint at group 1.
        let mut groups: Vec<(u32, u32)> = module
            .global_variables
            .iter()
            .filter_map(|(_, variable)| variable.binding.as_ref())
            .map(|binding| (binding.group, binding.binding))
            .collect();
        groups.sort_unstable();
        assert_eq!(groups, [(0, 0), (1, 0)]);
    }

    #[test]
    fn the_vertex_layout_matches_the_struct() {
        assert_eq!(Vertex::LAYOUT.array_stride, 8);
        assert_eq!(Vertex::LAYOUT.attributes.len(), 1);
        assert_eq!(std::mem::size_of::<View>(), 16, "one 16-byte uniform row");
        assert_eq!(std::mem::size_of::<Paint>(), 16, "one 16-byte uniform row");
    }

    #[test]
    fn colours_are_linear_for_srgb_targets_and_encoded_otherwise() {
        let page = Rgb(0x2b, 0x2b, 0x2b);
        let [r, _, _, a] = page.for_format(wgpu::TextureFormat::Rgba8Unorm);
        assert!((r - 43.0 / 255.0).abs() < 1e-6, "{r}");
        assert!((a - 1.0).abs() < 1e-6);
        let [r, _, _, _] = page.for_format(wgpu::TextureFormat::Bgra8UnormSrgb);
        // sRGB 43/255 decodes to about 0.0242 linear.
        assert!((r - 0.024_158).abs() < 1e-4, "{r}");
        let clear = Rgb(255, 0, 0).clear_for_format(wgpu::TextureFormat::Bgra8UnormSrgb);
        assert!((clear.r - 1.0).abs() < 1e-9 && clear.g == 0.0 && clear.b == 0.0);
        assert_eq!(Rgb(0xf0, 0xc0, 0x30).to_css(), "#f0c030");
        assert_eq!(Rgb(0, 1, 255).to_css(), "#0001ff");
    }
}
