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
use kicad_core::model::{Board, NetId, Point};
use kicad_core::tessellate::{Fine, ItemMesh, ItemSet, Mesh, Tessellator};

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
    /// Every item's mesh, kept beside the stitched buffers: a refined
    /// region (`Scene::refine`) builds again only the curved ones it
    /// reaches, and the highlight takes a net's meshes from here.
    pub(crate) sets: Vec<SceneSet>,
}

/// The item meshes of one set of items of the board, and the slice they
/// make.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SceneSet {
    layer: Layer,
    paint: Paint,
    set: ItemSet,
    meshes: Vec<ItemMesh>,
}

impl Scene {
    /// Every layer of `board` the views can show; `None` for a board
    /// with neither outline nor copper.
    #[must_use]
    pub fn build(board: &Board) -> Option<Self> {
        let tessellator = tessellator(None);
        let stack = Stack::of(board);
        let bounds = tessellator
            .outline_bounds(board)
            .or_else(|| tessellator.mesh(board, ItemSet::Outline).bounds())
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
            sets: Vec::new(),
        };
        for layer in stack.order(Side::Front) {
            // A KiCad layer's key is its canonical name in the board's
            // table; a board without the layer gets no slice for it.
            let id = board.layer_named(&layer.to_string());
            let sets: Vec<(Paint, ItemSet)> = match (&layer, id) {
                (Layer::Board, _) => vec![(Paint::Layer, ItemSet::BoardFill)],
                (Layer::Outline, _) => vec![(Paint::Layer, ItemSet::Outline)],
                // Each layer's slices top-most first: the first to cover
                // a sample owns it (the module doc).
                (Layer::Holes, _) => {
                    vec![
                        (Paint::Layer, ItemSet::Rims),
                        (Paint::Page, ItemSet::AllHoles),
                    ]
                }
                (Layer::Copper(_), Some(id)) => vec![
                    // The holes first, in the page colour: a track
                    // ending in a via covers the hole the via's ring
                    // left open, and the hole must win.
                    (Paint::Page, ItemSet::Holes(id)),
                    (Paint::Layer, ItemSet::CopperRest(id)),
                    (Paint::Zone, ItemSet::Zones(id)),
                ],
                (Layer::Mask(_), Some(id)) => vec![(Paint::Layer, ItemSet::Mask(id))],
                (Layer::Paste(_), Some(id)) => vec![(Paint::Layer, ItemSet::Paste(id))],
                (Layer::Silk(_), Some(id)) => vec![
                    (Paint::Layer, ItemSet::Graphics(id)),
                    (Paint::Layer, ItemSet::Text(id)),
                ],
                (Layer::Courtyard(_), Some(id)) => vec![(Paint::Layer, ItemSet::Graphics(id))],
                (
                    Layer::Copper(_)
                    | Layer::Mask(_)
                    | Layer::Paste(_)
                    | Layer::Silk(_)
                    | Layer::Courtyard(_),
                    None,
                ) => Vec::new(),
            };
            for (paint, set) in sets {
                let meshes = tessellator
                    .item_meshes(board, set, None)
                    .into_iter()
                    .flatten()
                    .collect();
                scene.sets.push(SceneSet {
                    layer: layer.clone(),
                    paint,
                    set,
                    meshes,
                });
            }
        }
        scene.stitch();
        Some(scene)
    }

    /// The view `state` names, when the board has it.
    #[must_use]
    pub fn view(&self, state: &ViewState) -> Option<&View> {
        self.views.get(state.view.checked_sub(1)?)
    }

    /// Lay every set's item meshes end to end in the one vertex buffer and
    /// the one index buffer, positions made relative to the origin. A set
    /// makes the slice of its layer and paint; the sets that follow
    /// each other in the same slice (a silkscreen's graphics and text)
    /// share it, and a slice with nothing in it is not made.
    fn stitch(&mut self) {
        let meshes = self.sets.iter().flat_map(|set| &set.meshes);
        let (vertices, indices) = meshes.fold((0, 0), |(vertices, indices), item| {
            (
                vertices + item.mesh.positions.len(),
                indices + item.mesh.indices.len(),
            )
        });
        self.vertices.clear();
        self.indices.clear();
        self.slices.clear();
        self.vertices.reserve(vertices);
        self.indices.reserve(indices);
        for set in &self.sets {
            let start = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
            for item in &set.meshes {
                append_relative(
                    &mut self.vertices,
                    &mut self.indices,
                    &item.mesh,
                    self.origin,
                );
            }
            let end = u32::try_from(self.indices.len()).unwrap_or(u32::MAX);
            if end == start {
                continue;
            }
            match self.slices.last_mut() {
                Some(last)
                    if last.layer == set.layer
                        && last.paint == set.paint
                        && last.range.end == start =>
                {
                    last.range.end = end;
                }
                _ => self.slices.push(Slice {
                    layer: set.layer.clone(),
                    paint: set.paint,
                    range: start..end,
                }),
            }
        }
    }

    /// Build again, from `board`, every curved item `fine` reaches:
    /// `fine` is the tolerance and the region (millimetres) the curves
    /// inside it are flattened to, or `None` for the floor everywhere.
    /// Items the region does not reach keep their meshes; so does a fill
    /// of plain points. The buffers are laid out afresh, and `true` says
    /// they changed.
    pub fn refine(&mut self, board: &Board, fine: Option<Fine>) -> bool {
        let tessellator = tessellator(fine);
        let mut changed = false;
        for set in &mut self.sets {
            let fresh = tessellator.item_meshes(board, set.set, Some(&set.meshes));
            for (item, fresh) in set.meshes.iter_mut().zip(fresh) {
                if let Some(fresh) = fresh {
                    *item = fresh;
                    changed = true;
                }
            }
        }
        if changed {
            self.stitch();
        }
        changed
    }

    /// The meshes of `net`'s units in the `paint` slice of `layer`, in the
    /// board's frame: what the highlight lays over the scene, from the
    /// same meshes the scene was stitched from.
    #[must_use]
    pub fn net_mesh(&self, layer: &Layer, paint: Paint, net: NetId) -> Mesh {
        let mut mesh = Mesh::default();
        for set in self
            .sets
            .iter()
            .filter(|set| set.layer == *layer && set.paint == paint)
        {
            for item in set.meshes.iter().filter(|item| item.net == Some(net)) {
                mesh.append(&item.mesh);
            }
        }
        mesh
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
                    Paint::Zone => (
                        view.color(&self.stack, &layer),
                        opacity * state.zone_opacity,
                    ),
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

/// The tessellator of a build: the floor tolerance everywhere, or finer
/// inside the region of `fine`.
pub(crate) fn tessellator(fine: Option<Fine>) -> Tessellator {
    Tessellator::new(TOLERANCE).with_fine(fine)
}

/// Add `mesh` after what the buffers hold, positions made relative to
/// `origin` and indices carried past the vertices already there.
pub(crate) fn append_relative(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    mesh: &Mesh,
    origin: Point,
) {
    let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
    vertices.extend(mesh.positions.iter().map(|position| Vertex {
        position: relative(*position, origin),
    }));
    indices.extend(mesh.indices.iter().map(|index| index + base));
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

    fn hand_off_board() -> &'static Board {
        static BOARD: OnceLock<Board> = OnceLock::new();
        BOARD.get_or_init(|| {
            let source =
                std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
            kicad_core::parse_board(&source).expect("the hand-off board parses")
        })
    }

    /// A square of `half` millimetres around the first via of the board.
    fn around_first_via(half: f64) -> Bounds {
        let at = hand_off_board().vias.first().expect("a via").at;
        Bounds {
            min: Point {
                x: at.x - half,
                y: at.y - half,
            },
            max: Point {
                x: at.x + half,
                y: at.y + half,
            },
        }
    }

    #[test]
    fn no_region_changes_nothing() {
        let mut scene = hand_off().clone();
        assert!(!scene.refine(hand_off_board(), None));
        assert_eq!(&scene, hand_off());
    }

    #[test]
    fn a_region_builds_the_curves_it_reaches_finer_and_leaving_it_restores_the_scene() {
        let mut scene = hand_off().clone();
        let before = scene.vertices.len();
        assert!(scene.refine(
            hand_off_board(),
            Some(Fine {
                tolerance: 0.5 / 1024.0,
                region: around_first_via(5.0)
            })
        ));
        assert!(scene.vertices.len() > before);
        // The slices still tile the index buffer.
        let mut next = 0;
        for slice in &scene.slices {
            assert_eq!(slice.range.start, next, "{slice:?}");
            next = slice.range.end;
        }
        assert_eq!(usize::try_from(next).expect("fits"), scene.indices.len());
        // The same region again builds the same scene.
        let refined = scene.clone();
        scene.refine(
            hand_off_board(),
            Some(Fine {
                tolerance: 0.5 / 1024.0,
                region: around_first_via(5.0),
            }),
        );
        assert_eq!(scene, refined);
        // Out of the region everything is as it was at load.
        assert!(scene.refine(hand_off_board(), None));
        assert_eq!(&scene, hand_off());
    }

    #[test]
    fn a_lit_nets_via_has_the_scenes_chords() {
        let board = hand_off_board();
        let via = board.vias.first().expect("a via");
        let net = via.net.expect("on a net");
        let layer = Layer::Copper("F.Cu".to_owned());
        let mut scene = hand_off().clone();
        let floor = scene.net_mesh(&layer, Paint::Layer, net);
        scene.refine(
            board,
            Some(Fine {
                tolerance: 0.5 / 1024.0,
                region: around_first_via(5.0),
            }),
        );
        let fine = scene.net_mesh(&layer, Paint::Layer, net);
        assert!(fine.positions.len() > floor.positions.len());
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
        state.zone_opacity = 1.0;
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
