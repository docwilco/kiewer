//! The label pass: the third pipeline, drawing the net labels of
//! `crate::labels` as instanced glyph meshes over everything else. Same
//! camera uniform at group 0, its own uniform at group 1 (the gate, the
//! class toggles, the colours), straight alpha blending; still the
//! WebGL2 floor (instancing is core there, a uniform is a uniform).
//!
//! The geometry is uploaded once per board ([`LabelPass::upload`]); what
//! changes per frame is one [`LabelFrame`] ([`LabelPass::set_frame`]):
//! the scale, the threshold, the toggles, the mirror, and which copper
//! layers are shown in what order. The gate itself runs in the vertex
//! shader (`labels.wgsl`), so hiding labels costs the CPU nothing per
//! frame beyond the uniform write.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;

use crate::labels::{self, GlyphSet, GlyphVertex, Group, LabelBatch, LabelInstance};
use crate::view::{Layer, palette};

/// The label shader, compiled at pipeline creation.
pub const SHADER: &str = include_str!("labels.wgsl");

/// How far past the threshold a label is fully opaque, as a factor of
/// it: the fade runs over the next 50%.
pub const FADE_END: f32 = 1.5;

/// What one frame needs from the viewer.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelFrame {
    /// Device pixels per millimetre.
    pub px_per_mm: f32,
    /// The text height on screen, in pixels, a label starts to show at.
    pub threshold_px: f32,
    /// Whether the view is mirrored.
    pub mirrored: bool,
    /// Whether each class is shown, in [`LabelClass::ALL`] order.
    ///
    /// [`LabelClass::ALL`]: kicad_core::labels::LabelClass::ALL
    pub classes: [bool; 5],
    /// The copper layers whose labels draw, bottom first.
    pub layers: Vec<Layer>,
    /// What every label's opacity is multiplied by: 1 as usual, the
    /// highlight's dim while a net is lit (`crate::highlight`), so the
    /// labels dim with the copper they sit on.
    pub dim: f32,
}

/// The uniform as the shader reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct Uniform {
    gate: [f32; 4],
    classes_a: [f32; 4],
    classes_b: [f32; 4],
    halo: [f32; 4],
    fill: [f32; 4],
}

/// The pipeline, its uniform, and the board's labels.
pub struct LabelPass {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    format: wgpu::TextureFormat,
    frame: Option<LabelFrame>,
    geometry: Option<Geometry>,
}

struct Geometry {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    instances: wgpu::Buffer,
    index_count: u32,
    instance_count: u32,
    glyphs: GlyphSet,
    groups: Vec<Group>,
}

impl LabelPass {
    /// A label pass on `device` for targets of `format` at `sample_count`
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
            label: Some("kiewer label shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("labels"),
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
            label: Some("labels"),
            contents: bytemuck::bytes_of(&Uniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("labels"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kiewer labels"),
            bind_group_layouts: &[Some(view_layout), Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("instanced labels"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(GlyphVertex::LAYOUT), Some(LabelInstance::LAYOUT)],
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
                // A mirrored glyph winds the other way.
                cull_mode: None,
                ..Default::default()
            },
            // Over the groups, joining none (`crate::render`).
            depth_stencil: Some(crate::render::overlay_depth_state()),
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
            format,
            frame: None,
            geometry: None,
        }
    }

    /// Replace the board's labels with `batch`; an empty batch draws
    /// nothing.
    pub fn upload(&mut self, device: &wgpu::Device, batch: &LabelBatch) {
        if batch.instances.is_empty() || batch.glyphs.indices.is_empty() {
            self.geometry = None;
            return;
        }
        let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("label glyphs"),
            contents: bytemuck::cast_slice(&batch.glyphs.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("label glyph indices"),
            contents: bytemuck::cast_slice(&batch.glyphs.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        let instances = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("label instances"),
            contents: bytemuck::cast_slice(&batch.instances),
            usage: wgpu::BufferUsages::VERTEX,
        });
        self.geometry = Some(Geometry {
            vertices,
            indices,
            instances,
            index_count: u32::try_from(batch.glyphs.indices.len()).unwrap_or(u32::MAX),
            instance_count: u32::try_from(batch.instances.len()).unwrap_or(u32::MAX),
            glyphs: batch.glyphs.clone(),
            groups: batch.groups.clone(),
        });
    }

    /// Drop the labels: a new board, or a sheet.
    pub fn clear(&mut self) {
        self.geometry = None;
    }

    /// What the next frames draw with; `None` draws no labels.
    pub fn set_frame(&mut self, frame: Option<LabelFrame>) {
        self.frame = frame;
    }

    /// Write the frame's uniform; before the pass that draws it.
    pub fn write(&self, queue: &wgpu::Queue) {
        let Some(frame) = &self.frame else {
            return;
        };
        let shown = |index: usize| {
            if frame.classes.get(index).copied().unwrap_or(false) {
                1.0
            } else {
                0.0
            }
        };
        let uniform = Uniform {
            gate: [
                frame.px_per_mm,
                frame.threshold_px,
                if frame.mirrored { 1.0 } else { 0.0 },
                FADE_END,
            ],
            classes_a: [shown(0), shown(1), shown(2), shown(3)],
            classes_b: [shown(4), 0.0, 0.0, 0.0],
            halo: dimmed(palette::LABEL_HALO.for_format(self.format), frame.dim),
            fill: dimmed(palette::LABEL_FILL.for_format(self.format), frame.dim),
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uniform));
    }

    /// Draw the frame's layers' labels into `pass`, whose group 0 holds
    /// the view.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_labels(pass);
    }
}

/// `color` with its alpha multiplied by `dim`, clamped to 0..=1.
fn dimmed(mut color: [f32; 4], dim: f32) -> [f32; 4] {
    color[3] *= dim.clamp(0.0, 1.0);
    color
}

impl LabelPass {
    fn draw_labels(&self, pass: &mut wgpu::RenderPass<'_>) {
        let (Some(frame), Some(geometry)) = (&self.frame, &self.geometry) else {
            return;
        };
        let draws = labels::draws(&geometry.glyphs, &geometry.groups, &frame.layers);
        if draws.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(1, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, geometry.vertices.slice(..));
        pass.set_vertex_buffer(1, geometry.instances.slice(..));
        pass.set_index_buffer(geometry.indices.slice(..), wgpu::IndexFormat::Uint32);
        for draw in draws {
            if draw.indices.end > geometry.index_count
                || draw.instances.end > geometry.instance_count
            {
                continue;
            }
            pass.draw_indexed(draw.indices, 0, draw.instances);
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    /// Where a glyph-local vector lands on the screen, in screen units,
    /// by the shader's placement: `local` flipped in x when mirrored,
    /// turned by the label's angle in KiCad's sense, then through the
    /// camera, whose x runs backwards when mirrored. A copy of the
    /// arithmetic of `vs_main`, kept here so the rule has a test.
    fn on_screen(angle: f64, mirrored: bool, local: [f64; 2]) -> [f64; 2] {
        let (s, c) = angle.to_radians().sin_cos();
        let x = if mirrored { -local[0] } else { local[0] };
        let y = local[1];
        let turned = [x * c + y * s, -x * s + y * c];
        let x_sign = if mirrored { -1.0 } else { 1.0 };
        [x_sign * turned[0], turned[1]]
    }

    #[test_case(0.0)]
    #[test_case(45.0)]
    #[test_case(-30.0)]
    #[test_case(90.0)]
    fn a_mirrored_label_reads_along_the_mirrored_direction_unmirrored(angle: f64) {
        let close =
            |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
        let reading = on_screen(angle, false, [1.0, 0.0]);
        let down = on_screen(angle, false, [0.0, 1.0]);
        // Mirrored, the track lies along the plain direction reflected in
        // x, and the text reads along it left to right: the reflection
        // negated, which is the plain direction at the negated angle.
        let mirrored_reading = on_screen(angle, true, [1.0, 0.0]);
        assert!(
            close(mirrored_reading, [reading[0], -reading[1]]),
            "{mirrored_reading:?}"
        );
        // And the glyphs are not mirrored: "down" stays a quarter turn
        // clockwise from "reading" on the screen, as it is unmirrored.
        let mirrored_down = on_screen(angle, true, [0.0, 1.0]);
        let handedness = |r: [f64; 2], d: [f64; 2]| r[0] * d[1] - r[1] * d[0];
        assert!((handedness(reading, down) - 1.0).abs() < 1e-9);
        assert!((handedness(mirrored_reading, mirrored_down) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_label_shader_parses_and_validates_with_its_uniform_at_group_one() {
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
        assert_eq!(groups, [(0, 0), (1, 0)]);
        // The vertex stage takes the glyph and the instance as one input
        // struct over seven locations.
        let vertex = module
            .entry_points
            .iter()
            .find(|entry| entry.name == "vs_main")
            .expect("vs_main");
        assert_eq!(vertex.function.arguments.len(), 1);
    }

    #[test]
    fn the_uniform_is_five_rows_and_the_shader_baseline_is_the_placement_constant() {
        assert_eq!(std::mem::size_of::<Uniform>(), 80);
        assert!(SHADER.contains("const BASELINE: f32 = 0.415;"));
        assert!((LabelBatch::baseline() - 0.415).abs() < 1e-6);
    }
}
