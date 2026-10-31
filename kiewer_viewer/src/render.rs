//! Drawing with wgpu at the WebGL2 floor (ADR 0001): one pipeline over
//! flat-coloured triangles, a vertex buffer and an index buffer, one
//! uniform for the camera. No storage buffers, no compute, so the same
//! code runs on WebGPU, on WebGL2 and natively.
//!
//! Nothing here knows about a canvas or a window: the surface and the
//! adapter are the platform's (`web.rs` in the browser); this module takes
//! a device, a format and a texture view to draw into.

use bytemuck::{Pod, Zeroable};
use derive_more::Display;
use wgpu::util::DeviceExt as _;

use crate::camera::View;

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

/// One vertex: a position in millimetres relative to the scene's origin,
/// and its colour as the target format takes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
    };
}

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
    geometry: Option<Geometry>,
    format: wgpu::TextureFormat,
    sample_count: u32,
    msaa: Option<MsaaTarget>,
}

struct Geometry {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kiewer viewer"),
            bind_group_layouts: &[Some(&bind_group_layout)],
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
                    blend: Some(wgpu::BlendState::REPLACE),
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
        Self {
            pipeline,
            uniform,
            bind_group,
            geometry: None,
            format,
            sample_count,
            msaa: None,
        }
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

    /// Draw the geometry as `view` sees it into `target`, a texture of
    /// this renderer's format and of `size` pixels, over `clear`.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        size: (u32, u32),
        view: View,
        clear: wgpu::Color,
    ) {
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&view));
        let msaa_view = (self.sample_count > 1).then(|| self.msaa_view(device, size));
        let (attachment, resolve_target) = match &msaa_view {
            Some(msaa) => (msaa, Some(target)),
            None => (target, None),
        };
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
                pass.draw_indexed(0..geometry.index_count, 0, 0..1);
            }
        }
        queue.submit(std::iter::once(encoder.finish()));
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
    }

    #[test]
    fn the_vertex_layout_matches_the_struct() {
        assert_eq!(Vertex::LAYOUT.array_stride, 24);
        assert_eq!(Vertex::LAYOUT.attributes.len(), 2);
        assert_eq!(Vertex::LAYOUT.attributes[1].offset, 8);
        assert_eq!(std::mem::size_of::<View>(), 16, "one 16-byte uniform row");
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
    }
}
