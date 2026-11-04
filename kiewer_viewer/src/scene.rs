//! A board as the renderer draws it: every layer the views can show,
//! tessellated once into one vertex buffer and one index buffer, each
//! layer a run of indices ([`Slice`]) so a toggle or an opacity is a
//! change to what is drawn and never a new tessellation. Positions are
//! relative to the board's centre so the GPU's f32 holds them to a few
//! nanometres. Colours are not here: a slice names its layer, and the
//! view in force colours it when the frame is drawn
//! ([`Scene::draw_list`]).
//!
//! A layer draws as one group (#56; `crate::render` has the mechanism):
//! its slices share one depth, so wherever two of its shapes overlap the
//! layer's opacity applies once, as the hand-off composes a translucent
//! layer. Inside a group the first shape to cover a sample owns it, so a
//! layer's slices are built and drawn top-most first: a copper layer's
//! holes, then its pads, vias and tracks, then its zones (at the zone
//! opacity, so a fill reads apart from the tracks on it); the holes
//! layer's rims, then its holes. A hole in a copper layer at 0.8 then
//! reads 0.8 of the page over what is under it, with no copper in it.

use std::ops::Range;

use kicad_core::geometry::Bounds;
use kicad_core::model::{Board, Point};
use kicad_core::tessellate::{Mesh, Tessellator};

use crate::labels::LabelBatch;
use crate::render::{Draw, Rgb, Vertex};
use crate::view::{Layer, Side, Stack, View, ViewState, palette};

/// How far a flattened curve may sit from the true one, in millimetres:
/// a micrometre, a tenth of a pixel at the thousandfold zoom the tracer
/// asks for.
pub const TOLERANCE: f64 = 0.001;

/// What a slice is coloured with when it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// The colour its layer has in the view in force.
    Layer,
    /// The page colour: a hole, drawn before the copper of its group so
    /// a track that runs into it does not fill it back in.
    Page,
    /// The layer's colour at the zone opacity: a copper layer's zones,
    /// drawn after its other copper, so a track or a pad on a fill owns
    /// its area and the fill shows around it.
    Zone,
}

/// One run of the index buffer: part of one layer, in one paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    /// The layer the run belongs to; what shows or hides it.
    pub layer: Layer,
    /// What colours it.
    pub paint: Paint,
    /// The indices, a multiple of three.
    pub range: Range<u32>,
}

/// Triangles ready for upload, with what each run of them is.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// The world point the positions are relative to: the middle of the
    /// outline's box (or of the copper's, for a board without an outline).
    pub origin: Point,
    /// What the camera fits to, in world millimetres.
    pub bounds: Bounds,
    /// Positions relative to [`Scene::origin`].
    pub vertices: Vec<Vertex>,
    /// Three per triangle.
    pub indices: Vec<u32>,
    /// The runs of [`Scene::indices`], in the order they were built;
    /// a frame draws them in the view's order.
    pub slices: Vec<Slice>,
    /// The board's copper, which names the views.
    pub stack: Stack,
    /// The stack's views ([`Stack::views`]), built once; a state's
    /// `view` numbers them from 1.
    pub views: Vec<View>,
    /// How many zones are drawn hatched for want of a stored fill.
    pub unfilled_zones: usize,
    /// The net labels, placed once (`crate::labels`), relative to
    /// [`Scene::origin`] like the vertices.
    pub labels: LabelBatch,
}

impl Scene {
    /// Every layer of `board` the views can show; `None` for a board
    /// with neither outline nor copper.
    #[must_use]
    pub fn build(board: &Board) -> Option<Self> {
        let tessellator = Tessellator::new(TOLERANCE);
        let stack = Stack::of(board);
        let bounds = tessellator
            .outline_bounds(board)
            .or_else(|| tessellator.outline(board).bounds())
            .or_else(|| {
                stack
                    .copper
                    .first()
                    .and_then(|name| board.layer_named(name))
                    .and_then(|layer| tessellator.copper(board, layer).bounds())
            })?;
        let mut scene = Self {
            origin: bounds.center(),
            bounds,
            vertices: Vec::new(),
            indices: Vec::new(),
            slices: Vec::new(),
            views: stack.views(),
            stack: stack.clone(),
            unfilled_zones: board.unfilled_zones(),
            labels: LabelBatch::build(board, bounds.center()),
        };
        for layer in stack.order(Side::Front) {
            // A KiCad layer's key is its canonical name in the board's
            // table; a board without the layer gets no slice for it.
            let id = board.layer_named(&layer.to_string());
            match (&layer, id) {
                (Layer::Board, _) => {
                    scene.add(&layer, Paint::Layer, &tessellator.board_fill(board));
                }
                (Layer::Outline, _) => {
                    scene.add(&layer, Paint::Layer, &tessellator.outline(board));
                }
                // Each layer's slices top-most first: the first to cover
                // a sample owns it (the module doc).
                (Layer::Holes, _) => {
                    scene.add(&layer, Paint::Layer, &tessellator.rims(board));
                    scene.add(&layer, Paint::Page, &tessellator.all_holes(board));
                }
                (Layer::Copper(_), Some(id)) => {
                    // The holes first, in the page colour: a track
                    // ending in a via covers the hole the via's ring
                    // left open, and the hole must win.
                    scene.add(&layer, Paint::Page, &tessellator.holes(board, id));
                    let rest = tessellator.copper_without_zones(board, id);
                    scene.add(&layer, Paint::Layer, &rest);
                    scene.add(&layer, Paint::Zone, &tessellator.zones(board, id));
                }
                (Layer::Mask(_), Some(id)) => {
                    scene.add(&layer, Paint::Layer, &tessellator.mask(board, id));
                }
                (Layer::Paste(_), Some(id)) => {
                    scene.add(&layer, Paint::Layer, &tessellator.paste(board, id));
                }
                (Layer::Silk(_), Some(id)) => {
                    let mut silk = tessellator.graphics(board, id);
                    silk.append(&tessellator.text(board, id));
                    scene.add(&layer, Paint::Layer, &silk);
                }
                (Layer::Courtyard(_), Some(id)) => {
                    scene.add(&layer, Paint::Layer, &tessellator.graphics(board, id));
                }
                (
                    Layer::Copper(_)
                    | Layer::Mask(_)
                    | Layer::Paste(_)
                    | Layer::Silk(_)
                    | Layer::Courtyard(_),
                    None,
                ) => {}
            }
        }
        Some(scene)
    }

    /// The view `state` names, when the board has it.
    #[must_use]
    pub fn view(&self, state: &ViewState) -> Option<&View> {
        self.views.get(state.view.checked_sub(1)?)
    }

    /// Append `mesh` as a slice of `layer` in `paint`, positions made
    /// relative to the origin. An empty mesh adds no slice.
    fn add(&mut self, layer: &Layer, paint: Paint, mesh: &Mesh) {
        if mesh.is_empty() {
            return;
        }
        let base = u32::try_from(self.vertices.len()).unwrap_or(u32::MAX);
        let start = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
        self.vertices
            .extend(mesh.positions.iter().map(|position| Vertex {
                position: relative(*position, self.origin),
            }));
        self.indices
            .extend(mesh.indices.iter().map(|index| index + base));
        let end = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
        self.slices.push(Slice {
            layer: layer.clone(),
            paint,
            range: start..end,
        });
    }

    /// The draws of one frame under `state`: every slice of every shown
    /// layer, in the view's draw order (bottom first), each layer its own
    /// group, coloured for a target of `format` with the layer's opacity
    /// as alpha, times the zone opacity for its zones.
    #[must_use]
    pub fn draw_list(&self, state: &ViewState, format: wgpu::TextureFormat) -> Vec<Draw> {
        let Some(view) = self.view(state) else {
            return Vec::new();
        };
        let mut draws = Vec::new();
        let mut group = 0;
        for layer in self.stack.order(view.top) {
            let Some(opacity) = state.opacity(view, &layer) else {
                continue;
            };
            for slice in self.slices.iter().filter(|slice| slice.layer == layer) {
                let (rgb, alpha) = match slice.paint {
                    Paint::Layer => (view.color(&self.stack, &layer), opacity),
                    Paint::Page => (palette::PAGE, opacity),
                    Paint::Zone => (view.color(&self.stack, &layer), opacity * state.zones),
                };
                let mut color = rgb.for_format(format);
                color[3] = alpha;
                draws.push(Draw {
                    range: slice.range.clone(),
                    color,
                    group,
                });
            }
            group += 1;
        }
        draws
    }
}

/// `position` (millimetres, f32) relative to `origin`: the difference is
/// taken in f64 so no precision is lost before the result is stored.
#[expect(
    clippy::cast_possible_truncation,
    reason = "relative to the board's centre a position is under a few hundred millimetres, which f32 holds to ten nanometres"
)]
fn relative(position: [f32; 2], origin: Point) -> [f32; 2] {
    [
        (f64::from(position[0]) - origin.x) as f32,
        (f64::from(position[1]) - origin.y) as f32,
    ]
}

/// The page colour as the frame is cleared to, for a target of `format`.
#[must_use]
pub fn clear_color(format: wgpu::TextureFormat) -> wgpu::Color {
    Rgb::clear_for_format(palette::PAGE, format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off() -> &'static Scene {
        static SCENE: OnceLock<Scene> = OnceLock::new();
        SCENE.get_or_init(|| {
            let source =
                std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
            let board = kicad_core::parse_board(&source).expect("the hand-off board parses");
            Scene::build(&board).expect("a board with an outline")
        })
    }

    #[test]
    fn the_hand_off_board_becomes_a_scene_around_its_centre() {
        let scene = hand_off();
        // The outline runs from (50, 50) to (162, 108).
        assert!((scene.origin.x - 106.0).abs() < 1e-6, "{:?}", scene.origin);
        assert!((scene.origin.y - 79.0).abs() < 1e-6, "{:?}", scene.origin);
        assert!((scene.bounds.width() - 112.0).abs() < 1e-6);
        assert!((scene.bounds.height() - 58.0).abs() < 1e-6);
        assert_eq!(scene.indices.len() % 3, 0);
        assert!(scene.vertices.len() > 10_000);
        // Every position is within the board, relative to its centre.
        for vertex in &scene.vertices {
            assert!(vertex.position[0].abs() <= 57.0, "{:?}", vertex.position);
            assert!(vertex.position[1].abs() <= 30.0, "{:?}", vertex.position);
        }
        let max_index = scene.indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < scene.vertices.len());
        assert_eq!(scene.unfilled_zones, 0);
        assert_eq!(scene.stack.copper, ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
    }

    #[test]
    fn the_slices_tile_the_index_buffer_and_name_every_layer_the_board_has() {
        let scene = hand_off();
        let mut next = 0;
        for slice in &scene.slices {
            assert_eq!(slice.range.start, next, "{slice:?}");
            assert!(slice.range.end > slice.range.start, "{slice:?}");
            assert_eq!((slice.range.end - slice.range.start) % 3, 0);
            next = slice.range.end;
        }
        assert_eq!(usize::try_from(next).expect("fits"), scene.indices.len());
        let has = |layer: Layer, paint: Paint| {
            scene
                .slices
                .iter()
                .any(|slice| slice.layer == layer && slice.paint == paint)
        };
        assert!(has(Layer::Copper("F.Cu".to_owned()), Paint::Zone));
        for name in ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"] {
            assert!(has(Layer::Copper(name.to_owned()), Paint::Layer), "{name}");
            assert!(
                has(Layer::Copper(name.to_owned()), Paint::Page),
                "{name} holes"
            );
        }
        assert!(has(Layer::Board, Paint::Layer));
        assert!(has(Layer::Outline, Paint::Layer));
        assert!(has(Layer::Mask(Side::Front), Paint::Layer));
        assert!(has(Layer::Mask(Side::Back), Paint::Layer));
        assert!(has(Layer::Paste(Side::Front), Paint::Layer));
        assert!(has(Layer::Silk(Side::Front), Paint::Layer));
        assert!(has(Layer::Courtyard(Side::Front), Paint::Layer));
        assert!(has(Layer::Holes, Paint::Page));
        assert!(has(Layer::Holes, Paint::Layer));
        // The hand-off board has nothing on the back silkscreen or the
        // back courtyard, so neither gets a slice.
        assert!(!has(Layer::Silk(Side::Back), Paint::Layer));
        assert!(!has(Layer::Courtyard(Side::Back), Paint::Layer));
    }

    #[test]
    fn the_front_as_built_draws_board_copper_mask_paste_silk_and_holes_in_that_order() {
        let scene = hand_off();
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let draws = scene.draw_list(&ViewState::new(5), format);
        let colors: Vec<([f32; 4], u32)> =
            draws.iter().map(|draw| (draw.color, draw.group)).collect();
        let rgba = |rgb: Rgb, alpha: f32, group: u32| {
            let mut color = rgb.for_format(format);
            color[3] = alpha;
            (color, group)
        };
        // One group per layer, its slices top-most first: the copper's
        // holes, the rest of it, its zones at the zone opacity; the rims
        // before the holes.
        assert_eq!(
            colors,
            [
                rgba(palette::BOARD, 1.0, 0),
                rgba(palette::PAGE, 1.0, 1),
                rgba(palette::COPPER, 1.0, 1),
                rgba(palette::COPPER, 0.66, 1),
                rgba(palette::MASK, 0.7, 2),
                rgba(palette::PASTE, 1.0, 3),
                rgba(palette::F_SILKS, 1.0, 4),
                rgba(palette::COPPER, 1.0, 5),
                rgba(palette::PAGE, 1.0, 5),
            ]
        );
        // Each draw is a slice's own range.
        for draw in &draws {
            assert!(scene.slices.iter().any(|slice| slice.range == draw.range));
        }
    }

    #[test]
    fn a_toggle_and_an_opacity_change_the_draws_and_nothing_else() {
        let scene = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let views = scene.stack.views();
        let mut state = ViewState::new(1);
        let plain = scene.draw_list(&state, format);
        // View 1: outline, F.Cu's holes, the rest of it and its zones,
        // F.Paste.
        assert_eq!(plain.len(), 5);
        state.set(&views[0], Layer::Paste(Side::Front), None);
        state.set(&views[0], Layer::Copper("F.Cu".to_owned()), Some(0.5));
        let changed = scene.draw_list(&state, format);
        assert_eq!(changed.len(), 4);
        assert!((changed[2].color[3] - 0.5).abs() < 1e-6, "{:?}", changed[2]);
        assert!(
            (changed[3].color[3] - 0.33).abs() < 1e-6,
            "{:?}",
            changed[3]
        );
        assert_eq!(changed[0], plain[0]);
        // The zone opacity moves the zones and nothing else.
        state.zones = 1.0;
        let opaque = scene.draw_list(&state, format);
        assert!((opaque[3].color[3] - 0.5).abs() < 1e-6, "{:?}", opaque[3]);
        assert_eq!(opaque[..3], changed[..3]);
    }

    #[test]
    fn the_back_as_built_draws_the_back_copper_over_the_front() {
        let scene = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let views = scene.stack.views();
        let mut state = ViewState::new(6);
        state.set(&views[5], Layer::Copper("F.Cu".to_owned()), Some(1.0));
        let draws = scene.draw_list(&state, format);
        let range_of = |layer: Layer| {
            scene
                .slices
                .iter()
                .find(|slice| slice.layer == layer && slice.paint == Paint::Layer)
                .map(|slice| slice.range.clone())
                .expect("the layer is in the scene")
        };
        let position = |layer: Layer| {
            let range = range_of(layer);
            draws
                .iter()
                .position(|draw| draw.range == range)
                .expect("drawn")
        };
        assert!(
            position(Layer::Copper("F.Cu".to_owned())) < position(Layer::Copper("B.Cu".to_owned()))
        );
        assert!(position(Layer::Copper("B.Cu".to_owned())) < position(Layer::Mask(Side::Back)));
    }

    #[test]
    fn a_board_with_nothing_to_draw_is_no_scene() {
        let board = kicad_core::parse_board(
            r#"(kicad_pcb (version 20241229) (generator "pcbnew") (layers (0 "F.Cu" signal)))"#,
        )
        .expect("parses");
        assert_eq!(Scene::build(&board), None);
    }
}
