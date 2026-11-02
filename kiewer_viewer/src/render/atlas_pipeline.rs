//! The text pass: the second pipeline beside the flat one, drawing the
//! glyph quads of `crate::atlas` over one coverage texture per face.
//! Same camera uniform at group 0, the face's texture and sampler at
//! group 1, straight alpha blending; still the WebGL2 floor (an R8
//! texture, a filtering sampler, nothing else).
//!
//! The pass is a list of faces, each with its atlas and its quads,
//! replaced whole when the page rasterises the face again. [`TextPass::draw`]
//! is one call the renderer makes wherever the text belongs in the
//! frame; it binds nothing but groups 1 and up, so the view at group 0
//! stays as the renderer set it.

use wgpu::util::DeviceExt as _;

use crate::atlas::TextVertex;

/// The text shader, compiled at pipeline creation.
pub const SHADER: &str = include_str!("text.wgsl");

/// A face's coverage as the page rasterised it: the atlas's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage<'a> {
    /// Texels across, a multiple of 256 (`write_texture`'s row alignment
    /// for one byte per texel).
    pub width: u32,
    /// Texels down.
    pub height: u32,
    /// `width * height` bytes, row-major, 255 for full coverage.
    pub bytes: &'a [u8],
}

/// The pipeline and the faces it draws.
pub struct TextPass {
    pipeline: wgpu::RenderPipeline,
    atlas_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    faces: Vec<FaceDraw>,
}

struct FaceDraw {
    face: String,
    bind_group: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

impl TextPass {
    /// A text pass on `device` for targets of `format` at `sample_count`
    /// samples per pixel, sharing the renderer's `view_layout` at
    /// group 0.
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        sample_count: u32,
        view_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kiewer text shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Linear in both directions: the atlas is rasterised within a
        // factor of four of the screen size (crate::atlas), where linear
        // filtering without mipmaps holds. Clamped: a cell's quad never
        // samples past the texture.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph atlas"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kiewer text"),
            bind_group_layouts: &[Some(view_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("textured text"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(TextVertex::LAYOUT)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // A mirrored text's quads wind the other way.
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
            atlas_layout,
            sampler,
            faces: Vec::new(),
        }
    }

    /// Replace `face`'s atlas and quads; with no indices the face is
    /// dropped from the pass.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        face: &str,
        coverage: &Coverage<'_>,
        vertices: &[TextVertex],
        indices: &[u32],
    ) {
        self.remove(face);
        if indices.is_empty() || coverage.width == 0 || coverage.height == 0 {
            return;
        }
        let size = wgpu::Extent3d {
            width: coverage.width,
            height: coverage.height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let expected = usize::try_from(u64::from(coverage.width) * u64::from(coverage.height))
            .unwrap_or(usize::MAX);
        // A short buffer is padded with no coverage rather than refused:
        // the page's canvas read-back is what it is.
        let mut padded;
        let bytes = if coverage.bytes.len() == expected {
            coverage.bytes
        } else {
            padded = coverage.bytes.to_vec();
            padded.resize(expected, 0);
            padded.as_slice()
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(coverage.width),
                rows_per_image: Some(coverage.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph atlas"),
            layout: &self.atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("text vertices"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("text indices"),
            contents: bytemuck::cast_slice(indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        self.faces.push(FaceDraw {
            face: face.to_owned(),
            bind_group,
            vertices: vertex_buffer,
            indices: index_buffer,
            index_count: u32::try_from(indices.len()).unwrap_or(u32::MAX),
        });
    }

    /// Drop `face` from the pass.
    pub fn remove(&mut self, face: &str) {
        self.faces.retain(|draw| draw.face != face);
    }

    /// Drop every face.
    pub fn clear(&mut self) {
        self.faces.clear();
    }

    /// Draw every face into `pass`, whose group 0 holds the view.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.faces.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        for face in &self.faces {
            pass.set_bind_group(1, &face.bind_group, &[]);
            pass.set_vertex_buffer(0, face.vertices.slice(..));
            pass.set_index_buffer(face.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..face.index_count, 0, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_shader_parses_and_validates_with_a_texture_at_group_one() {
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
        let groups: Vec<(u32, u32)> = module
            .global_variables
            .iter()
            .filter_map(|(_, var)| var.binding.as_ref())
            .map(|binding| (binding.group, binding.binding))
            .collect();
        assert_eq!(groups, [(0, 0), (1, 0), (1, 1)]);
    }
}
