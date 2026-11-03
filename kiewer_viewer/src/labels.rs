//! Net labels as the renderer draws them (docs/v1-scope.md, Net labels):
//! the records `kicad_core::labels` placed, turned into what the label
//! pipeline (`render/label_pipeline.rs`) draws with instancing. Every
//! glyph the labels use is tessellated once, in em units, from its
//! Newstroke strokes ([`GlyphSet`]): a halo stroke and the glyph's own.
//! Every character of every label is one [`LabelInstance`]: where its
//! text is centred, where the glyph sits in the text, how tall the text
//! is in millimetres and which way it reads. The vertex shader scales,
//! turns and places each glyph, and gates it on its height on screen,
//! so a zoom or a toggle changes nothing here.
//!
//! The instances are grouped by layer and character ([`Group`]), so a
//! frame draws one layer's labels as one indexed, instanced draw per
//! character: the halo of every glyph first, then the glyphs, so a
//! neighbour's halo never bites a glyph.

use std::collections::BTreeMap;
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use kicad_core::font;
use kicad_core::labels::{
    self, BASELINE_BELOW_CENTER, HALO_PER_HEIGHT, Label, LabelClass, PEN_PER_HEIGHT,
    glyph_positions,
};
use kicad_core::model::{Board, Point};
use kicad_core::tessellate::Tessellator;

use crate::view::Layer;

/// How far a glyph's flattened curve may sit from the true one, in em: a
/// twentieth of a pixel at a hundred pixels of text, where the labels
/// are as large as anyone reads them.
pub const GLYPH_TOLERANCE: f64 = 0.005;

/// One vertex of a glyph's mesh: a position in em units (x from the
/// glyph's left bearing, y down from the baseline), and whether it is
/// of the halo stroke (1) or the glyph's own (0).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct GlyphVertex {
    pub position: [f32; 2],
    pub halo: f32,
}

impl GlyphVertex {
    /// The layout at vertex buffer slot 0, stepped per vertex.
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<GlyphVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32],
    };
}

/// One character of one label: what places a glyph's mesh.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct LabelInstance {
    /// The text's centre, millimetres relative to the scene's origin.
    pub center: [f32; 2],
    /// Where the glyph's left bearing sits from the text's centre, in
    /// em (text heights), along the reading direction.
    pub cursor: f32,
    /// The text height, millimetres.
    pub height: f32,
    /// The cosine and sine of the label's angle (KiCad's sense).
    pub direction: [f32; 2],
    /// The label's class, as [`LabelClass::ALL`] numbers it, for the
    /// toggle mask.
    pub class: f32,
    /// Padding to 32 bytes; always 0.
    pub padding: f32,
}

impl LabelInstance {
    /// The layout at vertex buffer slot 1, stepped per instance.
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<LabelInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![2 => Float32x2, 3 => Float32, 4 => Float32, 5 => Float32x2, 6 => Float32],
    };
}

/// A glyph's two runs of the index buffer: its halo and its strokes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphRanges {
    /// The halo stroke, drawn first.
    pub halo: Range<u32>,
    /// The glyph's own strokes.
    pub fill: Range<u32>,
}

/// Every glyph the labels use, tessellated once in em units.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlyphSet {
    /// Every vertex of every glyph.
    pub vertices: Vec<GlyphVertex>,
    /// Three per triangle, into [`GlyphSet::vertices`].
    pub indices: Vec<u32>,
    /// Each character's runs of the index buffer.
    pub ranges: BTreeMap<char, GlyphRanges>,
}

impl GlyphSet {
    /// The glyphs of `chars`: each one's strokes at the label pen and at
    /// the halo pen, as [`PEN_PER_HEIGHT`] and [`HALO_PER_HEIGHT`] say,
    /// with round ends.
    #[must_use]
    pub fn build(chars: impl IntoIterator<Item = char>) -> Self {
        let tessellator = Tessellator::new(GLYPH_TOLERANCE);
        let mut set = Self::default();
        for c in chars {
            if set.ranges.contains_key(&c) {
                continue;
            }
            let strokes = font::glyph(c).strokes;
            let halo = set.append(&tessellator.polylines(&strokes, HALO_PER_HEIGHT), 1.0);
            let fill = set.append(&tessellator.polylines(&strokes, PEN_PER_HEIGHT), 0.0);
            set.ranges.insert(c, GlyphRanges { halo, fill });
        }
        set
    }

    /// Append a mesh with `halo` on every vertex; its index range.
    fn append(&mut self, mesh: &kicad_core::tessellate::Mesh, halo: f32) -> Range<u32> {
        let base = u32::try_from(self.vertices.len()).unwrap_or(u32::MAX);
        let start = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
        self.vertices.extend(
            mesh.positions
                .iter()
                .map(|&position| GlyphVertex { position, halo }),
        );
        self.indices
            .extend(mesh.indices.iter().map(|index| index + base));
        let end = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
        start..end
    }
}

/// The instances of one character on one layer: one instanced draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The copper layer the labels are on.
    pub layer: Layer,
    /// The character.
    pub glyph: char,
    /// The run of [`LabelBatch::instances`].
    pub instances: Range<u32>,
}

/// A board's labels, ready for the label pipeline.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LabelBatch {
    /// The labels as placed and resolved, for whoever asks what a label
    /// says and where (the net highlight builds on these).
    pub labels: Vec<Label>,
    /// The glyphs the labels use.
    pub glyphs: GlyphSet,
    /// One per character of every label, grouped as [`LabelBatch::groups`]
    /// says.
    pub instances: Vec<LabelInstance>,
    /// The groups, by layer in the board's copper order and by character.
    pub groups: Vec<Group>,
}

impl LabelBatch {
    /// The labels of `board` ([`labels::labels`]) as instances relative
    /// to `origin`.
    #[must_use]
    pub fn build(board: &Board, origin: Point) -> Self {
        Self::of(labels::labels(board), board, origin)
    }

    /// `labels` as instances relative to `origin`; `board` names the
    /// layers.
    #[must_use]
    pub fn of(labels: Vec<Label>, board: &Board, origin: Point) -> Self {
        let mut by_group: BTreeMap<(usize, char), Vec<LabelInstance>> = BTreeMap::new();
        let mut chars = Vec::new();
        for label in &labels {
            let (sin, cos) = label.angle.to_radians().sin_cos();
            let class = LabelClass::ALL
                .iter()
                .position(|&class| class == label.class)
                .unwrap_or(0);
            for (c, cursor) in glyph_positions(&label.text, label.height) {
                chars.push(c);
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "millimetres relative to the board's centre, em offsets and a height: all small and exact enough in f32"
                )]
                #[expect(clippy::cast_precision_loss, reason = "five classes")]
                let instance = LabelInstance {
                    center: [
                        (label.position.x - origin.x) as f32,
                        (label.position.y - origin.y) as f32,
                    ],
                    cursor: ((cursor - label.length / 2.0) / label.height) as f32,
                    height: label.height as f32,
                    direction: [cos as f32, sin as f32],
                    class: class as f32,
                    padding: 0.0,
                };
                by_group
                    .entry((label.layer.index(), c))
                    .or_default()
                    .push(instance);
            }
        }
        let mut instances = Vec::new();
        let mut groups = Vec::new();
        for ((layer, glyph), group) in by_group {
            let Some(name) = board.layers.get(layer).map(|layer| layer.name.clone()) else {
                continue;
            };
            let start = u32::try_from(instances.len()).unwrap_or(u32::MAX);
            instances.extend(group);
            let end = u32::try_from(instances.len()).unwrap_or(u32::MAX);
            groups.push(Group {
                layer: Layer::Copper(name),
                glyph,
                instances: start..end,
            });
        }
        Self {
            labels,
            glyphs: GlyphSet::build(chars),
            instances,
            groups,
        }
    }

    /// The baseline's offset below a label's centre, in em: what the
    /// shader adds to every glyph's y.
    #[must_use]
    #[expect(clippy::cast_possible_truncation, reason = "a constant near 0.4")]
    pub fn baseline() -> f32 {
        BASELINE_BELOW_CENTER as f32
    }
}

/// One instanced draw of the label pipeline: a glyph's index range over
/// a run of instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDraw {
    /// The glyph's halo or fill indices.
    pub indices: Range<u32>,
    /// The instances.
    pub instances: Range<u32>,
}

/// The draws of one frame for `layers` in draw order (bottom first):
/// per layer, every group's halo, then every group's fill.
#[must_use]
pub fn draws(glyphs: &GlyphSet, groups: &[Group], layers: &[Layer]) -> Vec<LabelDraw> {
    let mut out = Vec::new();
    for layer in layers {
        let on_layer = groups.iter().filter(|group| &group.layer == layer);
        for pick in [
            |ranges: &GlyphRanges| ranges.halo.clone(),
            |ranges: &GlyphRanges| ranges.fill.clone(),
        ] {
            for group in on_layer.clone() {
                if let Some(ranges) = glyphs.ranges.get(&group.glyph) {
                    out.push(LabelDraw {
                        indices: pick(ranges),
                        instances: group.instances.clone(),
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use kiewer_assert::assert_is_empty;

    use super::*;

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off() -> &'static (Board, LabelBatch) {
        static BATCH: OnceLock<(Board, LabelBatch)> = OnceLock::new();
        BATCH.get_or_init(|| {
            let source = std::fs::read_to_string(BOARD_PATH).expect("the hand-off board");
            let board = kicad_core::parse_board(&source).expect("parses");
            let batch = LabelBatch::build(&board, Point { x: 106.0, y: 79.0 });
            (board, batch)
        })
    }

    #[test]
    fn the_vertex_layouts_match_the_structs() {
        assert_eq!(GlyphVertex::LAYOUT.array_stride, 12);
        assert_eq!(GlyphVertex::LAYOUT.attributes.len(), 2);
        assert_eq!(LabelInstance::LAYOUT.array_stride, 32);
        assert_eq!(LabelInstance::LAYOUT.attributes.len(), 5);
        assert_eq!(
            LabelInstance::LAYOUT.step_mode,
            wgpu::VertexStepMode::Instance
        );
        // The two layouts share no shader location.
        let mut locations: Vec<u32> = GlyphVertex::LAYOUT
            .attributes
            .iter()
            .chain(LabelInstance::LAYOUT.attributes)
            .map(|attribute| attribute.shader_location)
            .collect();
        locations.sort_unstable();
        assert_eq!(locations, [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_glyph_set_strokes_each_character_once_with_a_halo_and_a_fill() {
        let set = GlyphSet::build("GND".chars().chain("DN".chars()));
        assert_eq!(set.ranges.len(), 3);
        let g = set.ranges.get(&'G').expect("G");
        assert!(!g.halo.is_empty() && !g.fill.is_empty());
        assert_eq!(g.halo.end, g.fill.start, "the halo comes first");
        assert!(
            g.halo.len() > g.fill.len(),
            "a wider stroke makes more triangles"
        );
        assert_eq!(set.indices.len() % 3, 0);
        let halos = set.vertices.iter().filter(|v| v.halo > 0.5).count();
        assert!(halos > 0 && halos < set.vertices.len());
        // Em units: a capital stays within about an em of the origin.
        for vertex in &set.vertices {
            assert!(
                vertex.position[0] > -0.5 && vertex.position[0] < 1.5,
                "{vertex:?}"
            );
            assert!(
                vertex.position[1] > -1.5 && vertex.position[1] < 0.5,
                "{vertex:?}"
            );
        }
    }

    #[test]
    fn a_label_becomes_one_instance_per_glyph_centred_on_its_text() {
        let board = kicad_core::parse_board(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew") (layers (0 "F.Cu" signal)))"#,
        )
        .expect("parses");
        let label = Label::new(
            "A B",
            LabelClass::TrackNet,
            None,
            board.layer_named("F.Cu").expect("a layer"),
            Point { x: 12.0, y: 20.0 },
            90.0,
            2.0,
        );
        let batch = LabelBatch::of(vec![label.clone()], &board, Point { x: 10.0, y: 10.0 });
        assert_eq!(batch.instances.len(), 2, "the space is no glyph");
        assert_eq!(batch.groups.len(), 2);
        assert_eq!(batch.groups[0].layer, Layer::Copper("F.Cu".to_owned()));
        let a = batch.instances[0];
        let close = |value: f32, expected: f64| (f64::from(value) - expected).abs() < 1e-6;
        assert!(close(a.center[0], 2.0) && close(a.center[1], 10.0), "{a:?}");
        assert!(close(a.height, 2.0));
        // The first glyph starts half the text length before the centre,
        // in em: the length over the height.
        assert!(close(a.cursor, -label.length / 2.0 / 2.0), "{a:?}");
        assert!(close(a.direction[0], 0.0) && close(a.direction[1], 1.0));
        assert!(close(a.class, 3.0));
        let b = batch.instances[1];
        assert!(b.cursor > a.cursor);
        assert!((LabelBatch::baseline() - 0.415).abs() < 1e-6);
    }

    #[test]
    fn the_hand_off_board_groups_its_labels_by_layer_and_character() {
        let (board, batch) = hand_off();
        assert!(batch.labels.len() > 2000, "{}", batch.labels.len());
        let glyph_count: usize = batch
            .labels
            .iter()
            .map(|label| glyph_positions(&label.text, label.height).len())
            .sum();
        assert_eq!(batch.instances.len(), glyph_count);
        let total: u32 = batch
            .groups
            .iter()
            .map(|group| group.instances.end - group.instances.start)
            .sum();
        assert_eq!(usize::try_from(total).expect("fits"), batch.instances.len());
        // Groups are sorted by layer then character, and tile the instances.
        let mut next = 0;
        for pair in batch.groups.windows(2) {
            let [a, b] = pair else { unreachable!() };
            assert!((&a.layer, a.glyph) < (&b.layer, b.glyph) || a.layer != b.layer);
        }
        for group in &batch.groups {
            assert_eq!(group.instances.start, next);
            next = group.instances.end;
            assert!(batch.glyphs.ranges.contains_key(&group.glyph));
        }
        for (_, layer) in board.copper_layers() {
            assert!(
                batch
                    .groups
                    .iter()
                    .any(|group| group.layer == Layer::Copper(layer.name.clone())),
                "{}",
                layer.name
            );
        }
        // One layer's draws: a halo and a fill per group of that layer.
        let front = Layer::Copper("F.Cu".to_owned());
        let groups = batch.groups.iter().filter(|g| g.layer == front).count();
        let front_draws = draws(&batch.glyphs, &batch.groups, std::slice::from_ref(&front));
        assert_eq!(front_draws.len(), 2 * groups);
        assert!(
            front_draws[..groups].iter().all(|d| batch
                .glyphs
                .ranges
                .values()
                .any(|r| r.halo == d.indices))
        );
        assert!(
            front_draws[groups..].iter().all(|d| batch
                .glyphs
                .ranges
                .values()
                .any(|r| r.fill == d.indices))
        );
        assert_is_empty!(draws(&batch.glyphs, &batch.groups, &[]));
    }
}
