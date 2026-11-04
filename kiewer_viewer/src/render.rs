//! Drawing with wgpu at the WebGL2 floor (ADR 0001): one pipeline over
//! flat triangles, a vertex buffer and an index buffer, one uniform for
//! the camera and one for the paint of each draw (its colour and its
//! group's depth), blended over what is already there so a layer can be
//! translucent; beside it the text pass of [`atlas_pipeline`], textured
//! quads over a glyph atlas per face. No storage buffers, no compute, so
//! the same code runs on WebGPU, on WebGL2 and natively.
//!
//! A frame is a list of [`Draw`]s, each a run of the index buffer in one
//! colour, drawn back to front in the order given: the scene decides what
//! that order is and which runs are in it, this module only draws them.
//!
//! A layer draws as one group (#56), as the hand-off composes a
//! translucent one: wherever two of its shapes overlap, it blends once.
//! Each draw names its group, and a later group is nearer: the vertex
//! shader writes the group's depth as `z` into one depth attachment,
//! sampled as the colour is, under the test `LESS`. A group still blends
//! over the groups before it, and inside one group the first shape to
//! cover a sample owns it, since every later shape of the group has the
//! equal depth and fails there; that is why the scene draws a layer's
//! slices top-most first (`crate::scene`). No stencil and no offscreen
//! target per layer, which would cost a composite per layer per frame.
//! The text and the net labels take the attachment with the compare
//! `Always` and write nothing ([`overlay_depth`]): a glyph's ink is its
//! alpha, which a depth write ignores, so a glyph quad in a group would
//! claim its whole box and the silk under its clear part would vanish.
//!
//! Nothing here knows about a canvas or a window: the surface and the
//! adapter are the platform's (`web.rs` in the browser); this module takes
//! a device, a format and a texture view to draw into.

pub mod atlas_pipeline;
pub mod label_pipeline;
#[cfg(not(target_arch = "wasm32"))]
pub mod offscreen;

use std::num::NonZeroU64;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use derive_more::Display;
use wgpu::util::DeviceExt as _;

use crate::atlas::TextVertex;
use crate::camera::View;
use crate::labels::LabelBatch;
use crate::render::atlas_pipeline::{Coverage, TextPass};
use crate::render::label_pipeline::{LabelFrame, LabelPass};

/// The shader source, compiled at pipeline creation.
pub const SHADER: &str = include_str!("shader.wgsl");

/// The limits every device is asked for: WebGL2's, so what runs on
/// WebGPU is what runs on WebGL2. They have no storage buffers and no
/// compute. Their one exception, `max_texture_dimension_2d`, is
/// [`device_descriptor_for`]'s.
#[must_use]
pub fn limits() -> wgpu::Limits {
    wgpu::Limits::downlevel_webgl2_defaults()
}

/// A canvas of `width` by `height` device pixels as a device whose
/// `max_texture_dimension_2d` is `limit` can draw it: as asked when both
/// sides are within the limit, otherwise scaled down with its aspect
/// ratio kept until the longer side is the limit, for the browser to
/// stretch to the displayed size. A side is never under one pixel. A
/// surface configured past the limit is a validation failure that wgpu
/// makes a panic, so every configure goes through this.
#[must_use]
pub fn cap_canvas(width: u32, height: u32, limit: u32) -> (u32, u32) {
    let (width, height, limit) = (width.max(1), height.max(1), limit.max(1));
    let longer = width.max(height);
    if longer <= limit {
        return (width, height);
    }
    // Rounded to the nearest pixel, in u64 so the product cannot wrap; the
    // result is at most `limit`, so it fits back in a u32.
    let side = |side: u32| {
        let scaled =
            (u64::from(side) * u64::from(limit) + u64::from(longer) / 2) / u64::from(longer);
        u32::try_from(scaled).unwrap_or(limit).max(1)
    };
    (side(width), side(height))
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

/// [`device_descriptor`] with `adapter`'s own `max_texture_dimension_2d`,
/// the one exception to the floor (ADR 0001): the device the browser's
/// canvas and the headless frame are drawn with. A request past the
/// adapter's limit fails, so the limit is the adapter's and not a number
/// picked here; a canvas past it is held to it by [`cap_canvas`].
#[must_use]
pub fn device_descriptor_for(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    let mut descriptor = device_descriptor();
    descriptor.required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d;
    descriptor
}

/// Which API a viewer draws with, for the page and the beacon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
pub enum Backend {
    #[display("webgpu")]
    WebGpu,
    #[display("webgl2")]
    WebGl2,
    /// Native, headless: lavapipe or a GPU's own driver (`headless`).
    #[display("vulkan")]
    Vulkan,
}

/// The depth attachment's format: inside the WebGL2 floor, and fine
/// enough for any count of groups a frame has.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;

/// The depth state of the flat triangles, which the groups are: a later
/// group is nearer and passes, and a later shape of the same group has
/// the equal depth and fails (the module doc).
fn group_test() -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: DEPTH_FORMAT,
        depth_write_enabled: Some(true),
        depth_compare: Some(wgpu::CompareFunction::Less),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

/// The depth state of a pass drawn over the groups without joining
/// them, the text and the net labels: every sample passes, none is
/// written.
#[must_use]
pub fn overlay_depth() -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: DEPTH_FORMAT,
        depth_write_enabled: Some(false),
        depth_compare: Some(wgpu::CompareFunction::Always),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

/// The depth group `group` of a frame with `groups` of them writes, in
/// 0 to 1 and nearer for a later group: evenly spaced inside the range,
/// so two groups are far more than a step of the attachment apart, and
/// the cleared 1 is behind them all.
#[must_use]
pub fn group_depth(group: u32, groups: u32) -> f32 {
    let depth = 1.0 - (f64::from(group) + 1.0) / (f64::from(groups) + 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a depth in 0 to 1, at least 2^-32 from the next group's, which f32 keeps apart"
    )]
    let depth = depth as f32;
    depth
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
/// layer's opacity as its alpha, as the target format takes it, in one
/// group.
#[derive(Debug, Clone, PartialEq)]
pub struct Draw {
    /// The indices, a multiple of three.
    pub range: Range<u32>,
    /// Red, green, blue as the shader writes them, and alpha.
    pub color: [f32; 4],
    /// The group the draw blends in (the module doc): a later group is
    /// nearer, and inside one the first draw to cover a sample owns it.
    /// A frame's groups count up from 0 in the order they are drawn.
    pub group: u32,
}

/// Where a frame goes: a texture of the renderer's format, its size in
/// pixels, and the colour it is cleared to first.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub view: &'a wgpu::TextureView,
    pub size: (u32, u32),
    pub clear: wgpu::Color,
}

/// The paint uniform of one draw, as the shader reads it: its colour
/// and its group's depth, padded to two 16-byte rows.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct Paint {
    color: [f32; 4],
    depth: f32,
    padding: [f32; 3],
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

    /// The colour at `lightness` 0 to 1, `chroma` and `hue` in degrees
    /// in OKLCH, as CSS `oklch()` means them, each sRGB channel clipped
    /// to its range as a browser clips one.
    #[must_use]
    pub fn oklch(lightness: f64, chroma: f64, hue: f64) -> Self {
        let (sin, cos) = hue.to_radians().sin_cos();
        let (a, b) = (chroma * cos, chroma * sin);
        let long = (lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b).powi(3);
        let medium = (lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b).powi(3);
        let short = (lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b).powi(3);
        Self(
            srgb_byte(4.076_741_662_1 * long - 3.307_711_591_3 * medium + 0.230_969_929_2 * short),
            srgb_byte(-1.268_438_004_6 * long + 2.609_757_401_1 * medium - 0.341_319_396_5 * short),
            srgb_byte(-0.004_196_086_3 * long - 0.703_418_614_7 * medium + 1.707_614_701_0 * short),
        )
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

/// A linear-light channel as an sRGB byte, clipped to its range.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=1 and scaled, the value is 0.5 to 255.5 before the cast"
)]
fn srgb_byte(linear: f64) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5) as u8
}

/// A change to the geometry past the first `keep_vertices` and
/// `keep_indices`: what follows them now. The scene's own geometry and
/// the nets in front of the change stay where they are on the GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct Splice {
    pub keep_vertices: u32,
    pub keep_indices: u32,
    pub vertices: Vec<Vertex>,
    /// Three per triangle, indexing the whole vertex buffer.
    pub indices: Vec<u32>,
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
    /// The depth attachment the groups are told apart by, sampled as the
    /// colour is.
    depth: Option<MsaaTarget>,
    /// Named-face text through the glyph atlas (`atlas_pipeline`), drawn
    /// after the flat geometry.
    text: TextPass,
    /// Net labels (`label_pipeline`), drawn last.
    labels: LabelPass,
}

/// What a geometry buffer needs beyond being read as vertices or
/// indices: [`Renderer::splice`] writes into it and copies out of it to
/// grow. Allowed beside an index buffer even at the WebGL2 floor.
const GEOMETRY_COPIES: wgpu::BufferUsages =
    wgpu::BufferUsages::COPY_DST.union(wgpu::BufferUsages::COPY_SRC);

/// One [`Vertex`], in bytes.
const VERTEX_SIZE: u64 = std::mem::size_of::<Vertex>() as u64;
/// One index, in bytes.
const INDEX_SIZE: u64 = std::mem::size_of::<u32>() as u64;

/// `buffer` when it holds `needed` bytes, otherwise a buffer of twice
/// that with its first `kept` bytes copied over.
fn with_room(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    kept: u64,
    needed: u64,
) -> wgpu::Buffer {
    if buffer.size() >= needed {
        return buffer.clone();
    }
    let grown = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("geometry"),
        size: needed * 2,
        usage: buffer.usage(),
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_buffer_to_buffer(buffer, 0, &grown, 0, kept);
    queue.submit([encoder.finish()]);
    grown
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

/// A texture a frame draws into beside its target, at the target's
/// size: the multisampled colour, or the depth.
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
                // The vertex stage reads the depth, the fragment the colour.
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
            depth_stencil: Some(group_test()),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        });
        let text = TextPass::new(device, format, sample_count, &bind_group_layout);
        let labels = LabelPass::new(device, format, sample_count, &bind_group_layout);
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
            depth: None,
            text,
            labels,
        }
    }

    /// Replace the net labels (`crate::labels`) with `batch`'s.
    pub fn upload_labels(&mut self, device: &wgpu::Device, batch: &LabelBatch) {
        self.labels.upload(device, batch);
    }

    /// Drop the net labels: a new board, or a sheet.
    pub fn clear_labels(&mut self) {
        self.labels.clear();
    }

    /// What the next frames draw the labels with: the gate, the toggles,
    /// the mirror and the layers shown; `None` draws none.
    pub fn set_label_frame(&mut self, frame: Option<LabelFrame>) {
        self.labels.set_frame(frame);
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
            usage: GEOMETRY_COPIES | wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("indices"),
            contents: bytemuck::cast_slice(indices),
            usage: GEOMETRY_COPIES | wgpu::BufferUsages::INDEX,
        });
        self.geometry = Some(Geometry {
            vertices: vertex_buffer,
            indices: index_buffer,
            index_count: u32::try_from(indices.len()).unwrap_or(u32::MAX),
        });
    }

    /// Apply `splice`: its geometry written past what it keeps, without
    /// touching what it keeps. A buffer too small grows to twice what
    /// the splice needs, so a run of hovers grows it once. Nothing
    /// without geometry to splice into.
    pub fn splice(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, splice: &Splice) {
        let Some(geometry) = &mut self.geometry else {
            return;
        };
        let vertices: &[u8] = bytemuck::cast_slice(&splice.vertices);
        let indices: &[u8] = bytemuck::cast_slice(&splice.indices);
        let vertices_at = u64::from(splice.keep_vertices) * VERTEX_SIZE;
        let indices_at = u64::from(splice.keep_indices) * INDEX_SIZE;
        geometry.vertices = with_room(
            device,
            queue,
            &geometry.vertices,
            vertices_at,
            vertices_at + vertices.len() as u64,
        );
        geometry.indices = with_room(
            device,
            queue,
            &geometry.indices,
            indices_at,
            indices_at + indices.len() as u64,
        );
        queue.write_buffer(&geometry.vertices, vertices_at, vertices);
        queue.write_buffer(&geometry.indices, indices_at, indices);
        geometry.index_count = splice
            .keep_indices
            .saturating_add(u32::try_from(splice.indices.len()).unwrap_or(u32::MAX));
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
        let depth_view = self.depth_view(device, target.size);
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
            let groups = draws
                .iter()
                .map(|draw| draw.group.saturating_add(1))
                .max()
                .unwrap_or(1);
            let mut packed = vec![0u8; draws.len() * PAINT_STRIDE];
            for (chunk, draw) in packed
                .as_chunks_mut::<PAINT_STRIDE>()
                .0
                .iter_mut()
                .zip(draws)
            {
                let paint = Paint {
                    color: draw.color,
                    depth: group_depth(draw.group, groups),
                    padding: [0.0; 3],
                };
                let bytes = bytemuck::bytes_of(&paint);
                if let Some(slot) = chunk.get_mut(..bytes.len()) {
                    slot.copy_from_slice(bytes);
                }
            }
            queue.write_buffer(&paints.buffer, 0, &packed);
        }
        self.labels.write(queue);
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
                // Cleared behind every group, and needed only while the
                // pass runs.
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
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
            // The net labels over everything, as the hand-off draws them.
            pass.set_bind_group(0, &self.bind_group, &[]);
            self.labels.draw(&mut pass);
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
        let (format, samples) = (self.format, self.sample_count);
        attachment(device, &mut self.msaa, size, format, samples, "msaa")
    }

    /// The depth attachment, sampled as the colour is, remade when the
    /// target's size changes.
    fn depth_view(&mut self, device: &wgpu::Device, size: (u32, u32)) -> wgpu::TextureView {
        let samples = self.sample_count;
        attachment(
            device,
            &mut self.depth,
            size,
            DEPTH_FORMAT,
            samples,
            "depth",
        )
    }
}

/// The view of `held` when it is `size`; otherwise of a new attachment of
/// `format` at `samples` per pixel, which `held` then keeps.
fn attachment(
    device: &wgpu::Device,
    held: &mut Option<MsaaTarget>,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    samples: u32,
    label: &str,
) -> wgpu::TextureView {
    if let Some(target) = held
        && target.size == size
    {
        return target.view.clone();
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: samples,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    *held = Some(MsaaTarget {
        size,
        view: view.clone(),
    });
    view
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case((2076, 1665), 4096, (2076, 1665); "the phone's canvas under a GPU's own limit")]
    #[test_case((2048, 2048), 2048, (2048, 2048); "a canvas at the limit")]
    #[test_case((2100, 1000), 2048, (2048, 975); "a wide canvas over the limit")]
    #[test_case((1000, 2100), 2048, (975, 2048); "a tall canvas over the limit")]
    #[test_case((2076, 1665), 2048, (2048, 1643); "the phone's canvas at the WebGL2 floor")]
    #[test_case((4096, 4096), 2048, (2048, 2048); "a square canvas over the limit")]
    #[test_case((100_000, 1), 2048, (2048, 1); "a sliver keeps one pixel")]
    #[test_case((0, 0), 2048, (1, 1); "an empty canvas is one pixel")]
    #[test_case((u32::MAX, u32::MAX), 16384, (16384, 16384); "the largest canvas")]
    fn a_canvas_is_capped_to_the_device_limit_with_its_aspect_kept(
        (width, height): (u32, u32),
        limit: u32,
        capped: (u32, u32),
    ) {
        assert_eq!(cap_canvas(width, height, limit), capped);
    }

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
        assert_eq!(std::mem::size_of::<Paint>(), 32, "two 16-byte uniform rows");
    }

    #[test]
    fn a_later_group_is_nearer_and_every_group_is_in_front_of_the_clear() {
        let groups = 40;
        let depths: Vec<f32> = (0..groups)
            .map(|group| group_depth(group, groups))
            .collect();
        assert!(
            depths.iter().all(|depth| *depth > 0.0 && *depth < 1.0),
            "{depths:?}"
        );
        // Apart by far more than a step of a 24-bit attachment.
        assert!(
            depths.windows(2).all(|pair| pair[0] - pair[1] > 1e-4),
            "{depths:?}"
        );
    }

    #[test_case(1.0, 0.0, 0.0, "#ffffff" ; "white")]
    #[test_case(0.0, 0.0, 0.0, "#000000" ; "black")]
    #[test_case(0.627_955, 0.257_683, 29.234, "#ff0000" ; "css red")]
    #[test_case(0.866_44, 0.294_827, 142.495, "#00ff00" ; "css lime")]
    #[test_case(0.452_014, 0.313_214, 264.052, "#0000ff" ; "css blue")]
    fn oklch_agrees_with_css(lightness: f64, chroma: f64, hue: f64, css: &str) {
        assert_eq!(Rgb::oklch(lightness, chroma, hue).to_css(), css);
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
