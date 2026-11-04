//! The net highlight of a board (docs/v1-scope.md, Net interaction): how
//! a frame shows the nets [`Pick`] lights. The hit test is
//! `kicad_core::hit`; this module keeps the hit test and the state, the
//! copper of each lit net tessellated once and cached, and turns a
//! scene's draw list into the dimmed one: every slice at the pick's dim
//! of its opacity (the page-coloured holes excepted, so a hole stays a
//! hole), then each lit net's copper on each shown copper layer as a
//! group of its own, nearer than the whole scene (`crate::render`): its
//! pads, vias and tracks, then its zones, as two runs so each takes its
//! own strength, the zones' times the zone opacity. A dimmed layer is
//! one blend at the dim of its opacity, overlaps included, and every pad
//! of a lit net reads the same, on a fill or off one. No
//! slice is re-tessellated: the dim is a paint change through the
//! per-draw colour, and the lit nets are extra geometry after the
//! scene's own ([`Tail`], shared with the sheet), of which a change
//! writes only what moved.
//!
//! A pin's colour is its slot's: slot 0 keeps the layer's colour and a
//! later slot generates one ([`Ramp`]).
//!
//! Where the camera goes is here too: a footprint found by its reference
//! is framed with [`FOOTPRINT_MARGIN`] free around it; a pad picked from
//! a net's list is panned to, and zoomed to only when it is under
//! [`PAD_MIN_PX`] across on screen, so a pick never throws the zoom away.

use std::collections::BTreeMap;
use std::ops::Range;

use kicad_core::geometry::Bounds;
use kicad_core::hit::{Hit, Index, Item, net_only};
use kicad_core::model::{Board, NetId, Point};
use kicad_core::tessellate::{Mesh, Tessellator};

use crate::camera::Camera;
use crate::pick::{OnNet, Pick};
use crate::render::{Draw, Rgb, Splice, Vertex, next_group};
use crate::scene::{Paint, Scene, TOLERANCE};
use crate::view::{Layer, ViewState};

/// How far from the pointer, in device pixels, an item is still hit: a
/// hairline track at low zoom.
pub const SLOP_PX: f64 = 3.0;
/// The viewport fraction left free on each side of a footprint framed
/// by the reference search.
pub const FOOTPRINT_MARGIN: f64 = 0.3;
/// The viewport fraction left free on each side of a pad the view zooms
/// to: the pad spans a tenth.
pub const PAD_MARGIN: f64 = 0.45;
/// A pad narrower than this on screen, in device pixels, is zoomed to
/// as well as panned to.
pub const PAD_MIN_PX: f64 = 24.0;

/// A shown copper layer below the top one draws a later slot's colour
/// at this fraction of its strength, so the side still shows. Slot 0
/// has a colour per layer and needs no such help.
pub const FAR_SIDE: f32 = 0.55;

/// A lightness and a chroma in OKLCH.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shade {
    pub lightness: f64,
    pub chroma: f64,
}

/// The colours of the pin slots from 1: hues a golden angle apart from
/// [`Ramp::FIRST_HUE`], the shade alternating so neighbours differ in
/// lightness too. Slot 0 is not here: it is the drawing's own colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    /// The shade of slots 1, 3, 5 and on.
    pub first: Shade,
    /// The shade of slots 2, 4, 6 and on.
    pub second: Shade,
}

impl Ramp {
    /// The hue of slot 1, in degrees.
    pub const FIRST_HUE: f64 = 205.0;
    /// The degrees from one slot's hue to the next's.
    pub const GOLDEN_ANGLE: f64 = 137.508;

    /// The ramp for a board's dark page.
    pub const BOARD: Self = Self {
        first: Shade {
            lightness: 0.86,
            chroma: 0.15,
        },
        second: Shade {
            lightness: 0.72,
            chroma: 0.17,
        },
    };

    /// The ramp for a sheet's light paper: the same hues, darker. Tuned
    /// on the paper: the palest of the first 16 has 3.9:1 against it,
    /// more than the 3.6:1 of KiCad's own wire green.
    pub const SHEET: Self = Self {
        first: Shade {
            lightness: 0.48,
            chroma: 0.15,
        },
        second: Shade {
            lightness: 0.54,
            chroma: 0.15,
        },
    };

    /// The colour of `slot`; `None` for slot 0, which has none of its
    /// own.
    #[must_use]
    pub fn color(&self, slot: usize) -> Option<Rgb> {
        let step = slot.checked_sub(1)?;
        let shade = if step % 2 == 0 {
            self.first
        } else {
            self.second
        };
        // The hue repeats nothing a person could tell apart within any
        // count of pins that fits a u32.
        let turns = f64::from(u32::try_from(step).unwrap_or(u32::MAX));
        let hue = (Self::FIRST_HUE + Self::GOLDEN_ANGLE * turns).rem_euclid(360.0);
        Some(Rgb::oklch(shade.lightness, shade.chroma, hue))
    }
}

/// Which part of a net's copper a run of its geometry is: each takes its
/// own strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Part {
    /// The net's zones.
    Zones,
    /// Its pads, vias and tracks.
    Rest,
}

/// A length as the GPU's buffers count it. A scene and its nets are far
/// short of `u32::MAX` vertices.
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// `from` moved `by` of the way to `to`, channel by channel.
fn mix(from: [f32; 4], to: [f32; 4], by: f32) -> [f32; 4] {
    let mut mixed = to;
    for (channel, start) in mixed.iter_mut().zip(from) {
        *channel = start + (*channel - start) * by;
    }
    mixed
}

/// One net the GPU holds after the scene's own geometry.
#[derive(Debug, Clone, PartialEq)]
struct HeldNet<N, K> {
    net: N,
    /// Where each of its meshes sits in the index buffer.
    runs: Vec<(K, Range<u32>)>,
    /// Where the next net's vertices start.
    vertices_end: u32,
    /// Where the next net's indices start.
    indices_end: u32,
}

/// The lit nets' geometry after a scene's own: each net's meshes by `K`,
/// built once, and which nets the GPU holds in which order. The pins
/// sit in front of the hovered nets, so a sweep of the hover leaves them
/// where they are: [`Tail::sync`] writes from the first net that differs
/// and no further back.
#[derive(Debug, Clone, PartialEq)]
pub struct Tail<N, K> {
    meshes: BTreeMap<N, Vec<(K, Mesh)>>,
    held: Vec<HeldNet<N, K>>,
}

impl<N, K> Default for Tail<N, K> {
    fn default() -> Self {
        Self {
            meshes: BTreeMap::new(),
            held: Vec::new(),
        }
    }
}

impl<N: Copy + Ord, K: Clone> Tail<N, K> {
    /// Build `net`'s meshes unless they are in hand.
    pub fn cache(&mut self, net: N, build: impl FnOnce() -> Vec<(K, Mesh)>) {
        self.meshes.entry(net).or_insert_with(build);
    }

    /// The GPU no longer holds any of it: the scene was uploaded afresh.
    pub fn invalidate(&mut self) {
        self.held.clear();
    }

    /// Where `net`'s meshes sit, once a sync gave them to the GPU.
    #[must_use]
    pub fn runs(&self, net: N) -> Option<&[(K, Range<u32>)]> {
        self.held
            .iter()
            .find(|held| held.net == net)
            .map(|held| held.runs.as_slice())
    }

    /// Make the GPU hold `nets` in order, each [`Tail::cache`]d before,
    /// after `scene` vertices and indices: what to write, when anything,
    /// positions relative to `origin` like the scene's own.
    pub fn sync(&mut self, nets: &[N], scene: (usize, usize), origin: Point) -> Option<Splice> {
        let kept = self
            .held
            .iter()
            .zip(nets)
            .take_while(|(held, net)| held.net == **net)
            .count();
        self.held.truncate(kept);
        let fresh = nets.get(kept..).filter(|fresh| !fresh.is_empty())?;
        let (keep_vertices, keep_indices) = self
            .held
            .last()
            .map_or((count(scene.0), count(scene.1)), |held| {
                (held.vertices_end, held.indices_end)
            });
        let mut splice = Splice {
            keep_vertices,
            keep_indices,
            vertices: Vec::new(),
            indices: Vec::new(),
        };
        for net in fresh {
            let mut runs = Vec::new();
            for (key, mesh) in self.meshes.get(net).into_iter().flatten() {
                let base = keep_vertices + count(splice.vertices.len());
                let start = keep_indices + count(splice.indices.len());
                splice
                    .vertices
                    .extend(mesh.positions.iter().map(|position| Vertex {
                        position: relative(*position, origin),
                    }));
                splice
                    .indices
                    .extend(mesh.indices.iter().map(|index| index + base));
                let end = keep_indices + count(splice.indices.len());
                runs.push((key.clone(), start..end));
            }
            self.held.push(HeldNet {
                net: *net,
                runs,
                vertices_end: keep_vertices + count(splice.vertices.len()),
                indices_end: keep_indices + count(splice.indices.len()),
            });
        }
        Some(splice)
    }
}

impl OnNet for Hit {
    type Net = NetId;

    fn net(self) -> NetId {
        self.net
    }
}

/// The highlight state of one board. The default is no board: nothing to
/// hit, nothing highlighted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Highlight {
    index: Index,
    pick: Pick<Hit>,
    /// Each lit net's copper by layer and part.
    tail: Tail<NetId, (Layer, Part)>,
}

impl Highlight {
    /// The highlight over `board`, nothing hovered or pinned.
    #[must_use]
    pub fn new(board: &Board) -> Self {
        Self {
            index: Index::build(board),
            ..Self::default()
        }
    }

    /// The hit test over the board.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// What is hovered and pinned, and what a frame lights.
    #[must_use]
    pub fn pick(&self) -> &Pick<Hit> {
        &self.pick
    }

    /// The same, to change.
    pub fn pick_mut(&mut self) -> &mut Pick<Hit> {
        &mut self.pick
    }

    /// The GPU no longer holds the highlight's geometry (the scene was
    /// uploaded afresh): the next [`Highlight::geometry`] gives it again.
    pub fn invalidate(&mut self) {
        self.tail.invalidate();
    }

    /// What the hovered item is, for the tooltip: `LED6 pad 3: GND`,
    /// `via: GND`, `track: GND`, `zone: GND`; empty over nothing.
    #[must_use]
    pub fn tooltip(&self, board: &Board) -> String {
        let Some(hit) = self.pick.hovered() else {
            return String::new();
        };
        let net = board.net(hit.net).map_or("", |net| net.name.as_str());
        let what = match hit.item {
            Item::Pad { footprint, pad } => {
                let footprint = board.footprints.get(footprint);
                let reference = footprint
                    .and_then(|footprint| footprint.reference())
                    .unwrap_or("?");
                let number = footprint
                    .and_then(|footprint| footprint.pads.get(pad))
                    .map_or("", |pad| pad.number.as_str());
                if number.is_empty() {
                    format!("{reference} pad")
                } else {
                    format!("{reference} pad {number}")
                }
            }
            Item::Via(_) => "via".to_owned(),
            Item::Segment(_) | Item::Arc(_) => "track".to_owned(),
            Item::Zone { .. } => "zone".to_owned(),
        };
        format!("{what}: {net}")
    }

    /// What the GPU should take for the next frame to hold the lit
    /// nets' copper after `scene`'s geometry; `None` when what it holds
    /// is right.
    pub fn geometry(&mut self, board: &Board, scene: &Scene) -> Option<Splice> {
        let nets: Vec<NetId> = self.pick.lit().iter().map(|lit| lit.net).collect();
        for &net in &nets {
            self.tail.cache(net, || net_copper(board, scene, net));
        }
        self.tail.sync(
            &nets,
            (scene.vertices.len(), scene.indices.len()),
            scene.origin,
        )
    }

    /// The draws of one frame: the scene's under `state` at the pick's
    /// dim of their opacity (a page-coloured slice stays as it is), then
    /// each lit net in pick order on each shown copper layer, bottom
    /// first, each a group of its own after the scene's, in its slot's
    /// colour, or between two while it moves slot: its pads, vias and
    /// tracks, then its zones at `state`'s zone opacity of their
    /// strength. A lit net [`Highlight::geometry`] has not given yet
    /// draws nothing.
    #[must_use]
    pub fn draws(
        &self,
        scene: &Scene,
        state: &ViewState,
        format: wgpu::TextureFormat,
    ) -> Vec<Draw> {
        let mut draws = scene.draw_list(state, format);
        for draw in &mut draws {
            if !is_page(scene, draw) {
                draw.color[3] *= self.pick.dim();
            }
        }
        let Some(view) = scene.view(state) else {
            return draws;
        };
        let shown = shown_copper(scene, state);
        let mut group = next_group(&draws);
        for lit in self.pick.lit() {
            let Some(runs) = self.tail.runs(lit.net) else {
                continue;
            };
            let (previous_slot, blend) = lit.blend();
            for layer in &shown {
                // A slot's colour on this layer, with what the far side
                // rule leaves of it as alpha.
                let paint = |slot: usize| {
                    let generated = Ramp::BOARD.color(slot);
                    let far = generated.is_some() && Some(layer) != shown.last();
                    let rgb = generated.unwrap_or_else(|| view.color(&scene.stack, layer));
                    let mut color = rgb.for_format(format);
                    color[3] = if far { FAR_SIDE } else { 1.0 };
                    color
                };
                let color = mix(paint(previous_slot), paint(lit.slot), blend);
                // The rest first: the first to cover a sample owns it.
                for (part, strength) in [
                    (Part::Rest, lit.strength()),
                    (Part::Zones, lit.zone_strength() * state.zone_opacity),
                ] {
                    let Some((_, range)) =
                        runs.iter().find(|(key, _)| *key == (layer.clone(), part))
                    else {
                        continue;
                    };
                    let mut color = color;
                    color[3] *= strength;
                    draws.push(Draw {
                        range: range.clone(),
                        color,
                        group,
                    });
                }
                group += 1;
            }
        }
        draws
    }

    /// The colours of `slot`'s swatch under `state`: a generated slot's
    /// one, and for slot 0 the colour of each shown copper layer, top
    /// first, so a view with two has two. With no copper shown, the
    /// front copper's.
    #[must_use]
    pub fn swatch(&self, scene: &Scene, state: &ViewState, slot: usize) -> Vec<Rgb> {
        if let Some(generated) = Ramp::BOARD.color(slot) {
            return vec![generated];
        }
        let Some(view) = scene.view(state) else {
            return Vec::new();
        };
        let mut layers = shown_copper(scene, state);
        layers.reverse();
        if layers.is_empty() {
            layers.extend(scene.stack.copper.first().cloned().map(Layer::Copper));
        }
        layers
            .iter()
            .map(|layer| view.color(&scene.stack, layer))
            .collect()
    }
}

/// `net`'s copper on each of `scene`'s copper layers, zones and the rest
/// apart; an empty mesh has no entry.
fn net_copper(board: &Board, scene: &Scene, net: NetId) -> Vec<((Layer, Part), Mesh)> {
    let copper = net_only(board, net);
    let tessellator = Tessellator::new(TOLERANCE);
    let mut meshes = Vec::new();
    for name in &scene.stack.copper {
        let Some(id) = board.layer_named(name) else {
            continue;
        };
        for (part, mesh) in [
            (Part::Zones, tessellator.zones(&copper, id)),
            (Part::Rest, tessellator.copper_without_zones(&copper, id)),
        ] {
            if !mesh.is_empty() {
                meshes.push(((Layer::Copper(name.clone()), part), mesh));
            }
        }
    }
    meshes
}

/// Whether `draw` is one of `scene`'s page-coloured slices (a hole),
/// which the dim leaves alone.
#[must_use]
pub fn is_page(scene: &Scene, draw: &Draw) -> bool {
    scene
        .slices
        .iter()
        .any(|slice| slice.range == draw.range && slice.paint == Paint::Page)
}

/// The copper layers `state` shows of `scene`'s view, in the view's draw
/// order, bottom first: where a highlight draws and what a hit test
/// sees (reversed, top first).
#[must_use]
pub fn shown_copper(scene: &Scene, state: &ViewState) -> Vec<Layer> {
    let Some(view) = scene.view(state) else {
        return Vec::new();
    };
    scene
        .stack
        .order(view.top)
        .into_iter()
        .filter(|layer| matches!(layer, Layer::Copper(_)) && state.opacity(view, layer).is_some())
        .collect()
}

/// `position` (millimetres, f32) relative to `origin`, the difference
/// taken in f64, as the scene makes its own vertices.
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

/// Look at a footprint found by its reference: framed with
/// [`FOOTPRINT_MARGIN`] free on each side.
pub fn go_to_footprint(camera: &mut Camera, bounds: Bounds) {
    camera.frame(bounds, FOOTPRINT_MARGIN);
}

/// Look at a pad picked from a net's list: panned to its centre at the
/// scale in force, and zoomed to [`PAD_MARGIN`] when it would be under
/// [`PAD_MIN_PX`] across on screen.
pub fn go_to_pad(camera: &mut Camera, bounds: Bounds) {
    let across = bounds.width().max(bounds.height()) * camera.scale();
    if across < PAD_MIN_PX {
        camera.frame(bounds, PAD_MARGIN);
    } else {
        camera.look_at(bounds.center(), camera.scale());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::OnceLock;

    use kicad_core::hit::{find_footprint, footprint_bounds, net_pads};
    use kicad_core::model::LayerId;
    use kiewer_assert::assert_is_not_empty;
    use test_case::test_case;

    use super::*;
    use crate::camera::Size;
    use crate::pick::Gesture;

    const BOARD_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_pcb"
    );

    fn hand_off() -> &'static (Board, Scene) {
        static HAND_OFF: OnceLock<(Board, Scene)> = OnceLock::new();
        HAND_OFF.get_or_init(|| {
            let source =
                std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
            let board = kicad_core::parse_board(&source).expect("the hand-off board parses");
            let scene = Scene::build(&board).expect("a board with an outline");
            (board, scene)
        })
    }

    fn f_cu(board: &Board) -> Vec<LayerId> {
        vec![board.layer_named("F.Cu").expect("F.Cu")]
    }

    /// The hit on LED6's pad 3 (GND) and on the first via (LED5's VDD).
    fn gnd_pad_and_vdd_via(board: &Board, highlight: &Highlight) -> (Hit, Hit) {
        let pad = highlight
            .index()
            .hit(
                Point {
                    x: 122.45,
                    y: 55.15,
                },
                0.0,
                &f_cu(board),
            )
            .expect("LED6's pad 3");
        let via = highlight
            .index()
            .hit(Point { x: 115.7, y: 51.8 }, 0.0, &f_cu(board))
            .expect("the via");
        assert_ne!(pad.net, via.net);
        (pad, via)
    }

    #[test]
    fn the_tooltip_names_the_item_and_its_net() {
        let (board, _) = hand_off();
        let mut highlight = Highlight::new(board);
        assert_eq!(highlight.tooltip(board), "");
        let (pad, via) = gnd_pad_and_vdd_via(board, &highlight);
        highlight.pick_mut().hover(Some(pad));
        assert_eq!(highlight.tooltip(board), "LED6 pad 3: GND");
        highlight.pick_mut().hover(Some(via));
        assert_eq!(highlight.tooltip(board), "via: Net-(LED5-VDD)");
        let track = highlight
            .index()
            .hit(Point { x: 110.0, y: 86.5 }, 0.0, &f_cu(board))
            .expect("the first segment in the file");
        highlight.pick_mut().hover(Some(track));
        assert!(highlight.tooltip(board).starts_with("track: "));
    }

    /// A highlight whose fades land at once, with `hits` pinned in turn.
    fn pinned(board: &Board, hits: &[Hit]) -> Highlight {
        let mut highlight = Highlight::new(board);
        highlight.pick_mut().set_instant(true);
        for hit in hits {
            highlight
                .pick_mut()
                .press(Gesture::ModifiedClick, Some(*hit));
        }
        highlight
    }

    /// The draws past the scene's own, the lit nets'.
    fn net_draws(scene: &Scene, draws: &[Draw]) -> Vec<Draw> {
        draws
            .iter()
            .filter(|draw| usize::try_from(draw.range.start).expect("fits") >= scene.indices.len())
            .cloned()
            .collect()
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-5
    }

    #[test]
    fn a_pin_dims_the_scene_to_30_and_adds_its_copper_on_the_shown_layers() {
        let (board, scene) = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let state = ViewState::new(5);
        let mut highlight = Highlight::new(board);
        highlight.pick_mut().set_instant(true);
        assert_eq!(
            highlight.geometry(board, scene),
            None,
            "the scene alone is right"
        );
        let plain = highlight.draws(scene, &state, format);
        assert_eq!(plain, scene.draw_list(&state, format));
        let (pad, _) = gnd_pad_and_vdd_via(board, &highlight);
        highlight.pick_mut().press(Gesture::Click, Some(pad));
        let splice = highlight
            .geometry(board, scene)
            .expect("the net's copper is appended");
        assert_eq!(
            usize::try_from(splice.keep_indices).expect("fits"),
            scene.indices.len()
        );
        assert_is_not_empty!(splice.vertices);
        assert_eq!(splice.indices.len() % 3, 0);
        let total = usize::try_from(splice.keep_vertices).expect("fits") + splice.vertices.len();
        let max_index = splice.indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < total);
        assert_eq!(highlight.geometry(board, scene), None, "uploaded once");
        let draws = highlight.draws(scene, &state, format);
        // View 5 shows one copper layer, F.Cu. GND has the rest and zones
        // there: two draws after the scene's, in the copper colour, the
        // rest solid and the zones at the zone opacity, one group nearer
        // than every one of the scene's.
        let lit = net_draws(scene, &draws);
        assert_eq!(lit.len(), 2, "{lit:?}");
        assert_eq!(draws.len(), plain.len() + 2);
        let copper = crate::view::palette::COPPER.for_format(format);
        for (draw, alpha) in lit.iter().zip([1.0, 0.66]) {
            for (channel, expected) in draw.color.iter().zip(copper).take(3) {
                assert!((channel - expected).abs() < 1e-6, "{draw:?}");
            }
            assert!(close(draw.color[3], alpha), "{draw:?}");
            assert!(draw.range.end > draw.range.start);
        }
        let scene_groups = plain.iter().map(|draw| draw.group).max();
        assert_eq!(lit[0].group, lit[1].group);
        assert!(Some(lit[0].group) > scene_groups);
        for (dimmed, original) in draws.iter().zip(&plain) {
            assert_eq!(dimmed.range, original.range);
            let expected = if is_page(scene, original) {
                original.color[3]
            } else {
                original.color[3] * 0.3
            };
            assert!((dimmed.color[3] - expected).abs() < 1e-6, "{dimmed:?}");
            assert_eq!(dimmed.color[..3], original.color[..3]);
        }
        // Clearing leaves the scene alone in the draws, and the GPU has
        // nothing more to take: the tail past the scene just goes unused.
        highlight.pick_mut().clear();
        assert_eq!(highlight.geometry(board, scene), None);
        assert_eq!(highlight.draws(scene, &state, format), plain);
        highlight.invalidate();
        assert_eq!(
            highlight.geometry(board, scene),
            None,
            "nothing lit, nothing to give"
        );
    }

    #[test]
    fn a_hover_draws_the_scene_at_60_and_the_zones_at_35_of_the_zone_opacity() {
        let (board, scene) = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let state = ViewState::new(5);
        let mut highlight = Highlight::new(board);
        highlight.pick_mut().set_instant(true);
        let (pad, _) = gnd_pad_and_vdd_via(board, &highlight);
        highlight.pick_mut().hover(Some(pad));
        highlight.geometry(board, scene);
        let draws = highlight.draws(scene, &state, format);
        let plain = scene.draw_list(&state, format);
        let dimmed = plain
            .iter()
            .position(|draw| !is_page(scene, draw))
            .expect("a slice that dims");
        assert!(close(draws[dimmed].color[3], plain[dimmed].color[3] * 0.6));
        let lit = net_draws(scene, &draws);
        let [rest, zones] = lit.as_slice() else {
            panic!("{lit:?}");
        };
        assert!(close(zones.color[3], 0.35 * 0.66), "{zones:?}");
        assert!(close(rest.color[3], 1.0), "{rest:?}");
    }

    #[test]
    fn a_later_slot_draws_the_far_side_at_55_and_slot_0_keeps_both_colours() {
        let (board, scene) = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        // View 7 shows both outer coppers, B.Cu under F.Cu.
        let state = ViewState::new(7);
        let highlight = {
            let probe = Highlight::new(board);
            let (gnd, vdd) = gnd_pad_and_vdd_via(board, &probe);
            let mut highlight = pinned(board, &[gnd, vdd]);
            highlight.geometry(board, scene);
            highlight
        };
        let draws = highlight.draws(scene, &state, format);
        let lit = net_draws(scene, &draws);
        // GND: the rest and zones on two layers, each at full strength in
        // its layer's colour, the zones at the zone opacity; a group per
        // layer.
        let (gnd, vdd) = lit.split_at(4);
        for (draw, alpha) in gnd.iter().zip([1.0, 0.66, 1.0, 0.66]) {
            assert!(close(draw.color[3], alpha), "{gnd:?}");
        }
        assert_ne!(gnd[0].color[..3], gnd[3].color[..3], "two layer colours");
        let groups: Vec<u32> = gnd.iter().map(|draw| draw.group).collect();
        assert_eq!(groups[0], groups[1]);
        assert_eq!(groups[2], groups[3]);
        assert!(groups[2] > groups[1]);
        // The via's net: one colour, the far side at 55%, the near solid.
        let generated = Ramp::BOARD.color(1).expect("slot 1").for_format(format);
        assert!(vdd.iter().all(|draw| draw.color[..3] == generated[..3]));
        let first = vdd.first().expect("a far side draw");
        let last = vdd.last().expect("a near side draw");
        assert!(close(first.color[3], 0.55), "{first:?}");
        assert!(close(last.color[3], 1.0), "{last:?}");
    }

    #[test]
    fn a_pin_moving_to_slot_0_blends_its_colour_on_the_way() {
        let (board, scene) = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let state = ViewState::new(5);
        let mut highlight = Highlight::new(board);
        let (gnd, vdd) = gnd_pad_and_vdd_via(board, &highlight);
        highlight
            .pick_mut()
            .press(Gesture::ModifiedClick, Some(gnd));
        highlight
            .pick_mut()
            .press(Gesture::ModifiedClick, Some(vdd));
        let mut now = 0.0;
        while highlight.pick().fading() {
            highlight.pick_mut().advance(now);
            now += 16.0;
        }
        highlight.pick_mut().press(Gesture::Click, Some(vdd));
        highlight.geometry(board, scene);
        let from = Ramp::BOARD.color(1).expect("slot 1").for_format(format);
        let to = crate::view::palette::COPPER.for_format(format);
        let first = net_draws(scene, &highlight.draws(scene, &state, format));
        // The pin is drawn first; the old slot 0 pin fades out after it.
        let start = first.first().expect("the net's draw");
        assert!(close(start.color[0], from[0]), "{start:?}");
        highlight.pick_mut().advance(1000.0);
        highlight.pick_mut().advance(1050.0);
        highlight.pick_mut().advance(1075.0);
        let halfway = net_draws(scene, &highlight.draws(scene, &state, format));
        let middle = halfway.first().expect("the net's draw");
        assert!(close(
            middle.color[0],
            from[0] + (to[0] - from[0]) * 91.0 / 150.0
        ));
    }

    #[test]
    fn a_hover_change_with_pins_writes_only_past_the_pins() {
        let (board, scene) = hand_off();
        let highlight = Highlight::new(board);
        let (gnd, vdd) = gnd_pad_and_vdd_via(board, &highlight);
        let mut highlight = pinned(board, &[gnd]);
        let pinned = highlight.geometry(board, scene).expect("the pin");
        let after_pin = pinned.keep_indices + u32::try_from(pinned.indices.len()).expect("fits");
        highlight.pick_mut().hover(Some(vdd));
        let splice = highlight.geometry(board, scene).expect("the preview");
        assert_eq!(splice.keep_indices, after_pin);
        highlight.pick_mut().hover(None);
        assert_eq!(
            highlight.geometry(board, scene),
            None,
            "only a shorter tail"
        );
    }

    #[test]
    fn a_swatch_has_one_colour_a_shown_copper_layer_and_a_generated_slot_one() {
        let (board, scene) = hand_off();
        let highlight = Highlight::new(board);
        let two = ViewState::new(7);
        assert_eq!(highlight.swatch(scene, &two, 0).len(), 2);
        assert_eq!(highlight.swatch(scene, &ViewState::new(5), 0).len(), 1);
        assert_eq!(
            highlight.swatch(scene, &two, 3),
            vec![Ramp::BOARD.color(3).expect("slot 3")]
        );
    }

    // The expected colours are what Chromium draws for the same oklch().
    #[test_case(Ramp::BOARD, 1, "#00ecff" ; "board slot 1 at hue 205")]
    #[test_case(Ramp::BOARD, 2, "#ea74c1" ; "board slot 2 at hue 342.5")]
    #[test_case(Ramp::SHEET, 1, "#007285" ; "sheet slot 1 at hue 205")]
    #[test_case(Ramp::SHEET, 2, "#a74586" ; "sheet slot 2 at hue 342.5")]
    fn the_ramp_makes_the_issues_colours(ramp: Ramp, slot: usize, css: &str) {
        assert_eq!(ramp.color(slot).expect("a slot").to_css(), css);
    }

    #[test]
    fn slot_0_has_no_generated_colour() {
        assert_eq!(Ramp::BOARD.color(0), None);
    }

    #[test]
    fn no_two_of_the_first_17_slots_share_a_colour() {
        let colours: BTreeSet<String> = (1..=17)
            .filter_map(|slot| Ramp::BOARD.color(slot))
            .map(Rgb::to_css)
            .collect();
        assert_eq!(colours.len(), 17);
    }

    #[test]
    fn a_footprint_is_framed_and_a_pad_is_panned_to_or_zoomed_to() {
        let (board, scene) = hand_off();
        let viewport = Size {
            width: 1600.0,
            height: 900.0,
        };
        let mut camera = Camera::new(viewport);
        camera.fit(scene.bounds);
        let fitted = camera.scale();
        let led6 = find_footprint(board, "led6").expect("LED6");
        let bounds = footprint_bounds(&board.footprints[led6]).expect("a box");
        go_to_footprint(&mut camera, bounds);
        assert_eq!(camera.center(), bounds.center());
        // The box spans the middle 40% of whichever side of the viewport
        // limits it.
        let fraction = (bounds.width() * camera.scale() / viewport.width)
            .max(bounds.height() * camera.scale() / viewport.height);
        assert!((fraction - 0.4).abs() < 1e-9, "{fraction}");
        assert!(camera.scale() > fitted);
        // A pad from the fitted view is small on screen: zoomed to.
        let (gnd, _) = gnd_pad_and_vdd_via(board, &Highlight::new(board));
        let pads = net_pads(board, gnd.net);
        let pad = pads.first().expect("GND has pads");
        camera.fit(scene.bounds);
        go_to_pad(&mut camera, pad.bounds);
        assert_eq!(camera.center(), pad.bounds.center());
        assert!(camera.scale() > fitted);
        let zoomed = camera.scale();
        // From a close view, only a pan.
        go_to_pad(&mut camera, pad.bounds);
        assert!((camera.scale() - zoomed).abs() < 1e-9);
        let other = pads.last().expect("GND has pads");
        go_to_pad(&mut camera, other.bounds);
        assert_eq!(camera.center(), other.bounds.center());
        assert!(
            (camera.scale() - zoomed).abs() < 1e-9,
            "a pan keeps the zoom"
        );
    }
}
