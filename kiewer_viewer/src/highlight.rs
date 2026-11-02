//! The net highlight (docs/v1-scope.md, Net interaction): which net the
//! pointer is over, which one a click pinned, and how a frame shows it.
//! The hit test is `kicad_core::hit`; this module keeps the state, the
//! copper of each highlighted net tessellated once and cached, and turns
//! a scene's draw list into the dimmed one: every slice at [`DIM`] of its
//! opacity (the page-coloured holes excepted, so a hole stays a hole),
//! then the net's copper on each shown copper layer in the layer's own
//! colour at full opacity. No slice is re-tessellated: the dim is a paint
//! change through the per-draw colour, and the highlight is extra
//! geometry appended after the scene's own, uploaded once per net
//! change.
//!
//! Where the camera goes is here too: a footprint found by its reference
//! is framed with [`FOOTPRINT_MARGIN`] free around it; a pad picked from
//! the net's list is panned to, and zoomed to only when it is under
//! [`PAD_MIN_PX`] across on screen, so a pick never throws the zoom away.
//!
//! The hovered-and-pinned state is a [`Pick`] over any kind of hit, so
//! the sheet's highlight (`crate::sheet_highlight`) keeps the same
//! model; the copper and the camera rules here are the board's.

use std::collections::BTreeMap;
use std::ops::Range;

use kicad_core::geometry::Bounds;
use kicad_core::hit::{Hit, Index, Item, net_only};
use kicad_core::model::{Board, NetId, Point};
use kicad_core::tessellate::{Mesh, Tessellator};

use crate::camera::Camera;
use crate::render::{Draw, Vertex};
use crate::scene::{Paint, Scene, TOLERANCE};
use crate::view::{Layer, ViewState};

/// What everything off the highlighted net draws at: this fraction of
/// its opacity.
pub const DIM: f32 = 0.3;
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

/// Where a net's copper on one layer sits in the index buffer after the
/// scene's own indices: the highlight's slice of that layer.
type HighlightRange = (Layer, Range<u32>);

/// What a hit is on: its net. The board's hits and the sheet's
/// (`crate::sheet_highlight`) both keep their state in a [`Pick`].
pub trait OnNet: Copy {
    /// The net's id.
    type Net: Copy + PartialEq + std::fmt::Debug;
    /// The net the hit is on.
    fn net(self) -> Self::Net;
}

impl OnNet for Hit {
    type Net = NetId;

    fn net(self) -> NetId {
        self.net
    }
}

/// The net under the pointer and the one a click pinned: the
/// interaction model of docs/v1-scope.md, Net interaction, for hits of
/// any kind. Hover names the net, a click pins it, a click on nothing
/// clears the pin, Escape clears both, and the pin outlives the pointer
/// leaving.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick<H: OnNet> {
    hovered: Option<H>,
    pinned: Option<H::Net>,
}

impl<H: OnNet> Default for Pick<H> {
    fn default() -> Self {
        Self {
            hovered: None,
            pinned: None,
        }
    }
}

impl<H: OnNet> Pick<H> {
    /// The net a frame highlights: the pinned one, else the hovered
    /// item's.
    #[must_use]
    pub fn active(&self) -> Option<H::Net> {
        self.pinned.or(self.hovered.map(OnNet::net))
    }

    /// What the pointer is over.
    #[must_use]
    pub fn hovered(&self) -> Option<H> {
        self.hovered
    }

    /// The net a click pinned.
    #[must_use]
    pub fn pinned(&self) -> Option<H::Net> {
        self.pinned
    }

    /// The pointer is over `hit` (or over nothing). Whether the net a
    /// frame highlights changed, so a frame is due.
    pub fn hover(&mut self, hit: Option<H>) -> bool {
        let before = self.active();
        self.hovered = hit;
        self.active() != before
    }

    /// A click on `hit` pins its net; a click on nothing clears the pin.
    /// Whether the net a frame highlights changed.
    pub fn pin(&mut self, hit: Option<H>) -> bool {
        let before = self.active();
        self.pinned = hit.map(OnNet::net);
        self.active() != before
    }

    /// Escape: nothing pinned, nothing hovered. Whether the net a frame
    /// highlights changed.
    pub fn clear(&mut self) -> bool {
        let before = self.active();
        self.pinned = None;
        self.hovered = None;
        before.is_some()
    }
}

/// The highlight state of one board. The default is no board: nothing to
/// hit, nothing highlighted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Highlight {
    index: Index,
    pick: Pick<Hit>,
    /// Each net's copper by layer, tessellated the first time it is
    /// highlighted.
    meshes: BTreeMap<NetId, Vec<(Layer, Mesh)>>,
    /// The net whose copper the GPU holds after the scene's own, and
    /// where each layer's run of it sits; `None` when the GPU holds the
    /// scene alone.
    uploaded: Option<(NetId, Vec<HighlightRange>)>,
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

    /// What every label's opacity is multiplied by this frame: [`DIM`]
    /// while a net is highlighted, 1 otherwise.
    #[must_use]
    pub fn dim(&self) -> f32 {
        if self.active().is_some() { DIM } else { 1.0 }
    }

    /// The hit test over the board.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The net a frame highlights: the pinned one, else the hovered
    /// item's.
    #[must_use]
    pub fn active(&self) -> Option<NetId> {
        self.pick.active()
    }

    /// What the pointer is over.
    #[must_use]
    pub fn hovered(&self) -> Option<Hit> {
        self.pick.hovered()
    }

    /// The net a click pinned.
    #[must_use]
    pub fn pinned(&self) -> Option<NetId> {
        self.pick.pinned()
    }

    /// The pointer is over `hit` (or over nothing). Whether the net a
    /// frame highlights changed, so a frame is due.
    pub fn hover(&mut self, hit: Option<Hit>) -> bool {
        self.pick.hover(hit)
    }

    /// A click on `hit` pins its net; a click on nothing clears the pin.
    /// Whether the net a frame highlights changed.
    pub fn pin(&mut self, hit: Option<Hit>) -> bool {
        self.pick.pin(hit)
    }

    /// Escape: nothing pinned, nothing hovered. Whether the net a frame
    /// highlights changed.
    pub fn clear(&mut self) -> bool {
        self.pick.clear()
    }

    /// The GPU no longer holds the highlight's geometry (the scene was
    /// uploaded afresh): the next [`Highlight::geometry`] gives it again.
    pub fn invalidate(&mut self) {
        self.uploaded = None;
    }

    /// What the hovered item is, for the tooltip: `LED6 pad 3: GND`,
    /// `via: GND`, `track: GND`, `zone: GND`; empty over nothing.
    #[must_use]
    pub fn tooltip(&self, board: &Board) -> String {
        let Some(hit) = self.hovered() else {
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

    /// The geometry the GPU should hold for the next frame when it does
    /// not already: `scene`'s vertices and indices with the active net's
    /// copper appended, positions relative to the scene's origin like
    /// the scene's own. `None` when what it holds is right.
    pub fn geometry(&mut self, board: &Board, scene: &Scene) -> Option<(Vec<Vertex>, Vec<u32>)> {
        let active = self.active();
        if self.uploaded.as_ref().map(|(net, _)| *net) == active {
            return None;
        }
        let Some(net) = active else {
            self.uploaded = None;
            return Some((scene.vertices.clone(), scene.indices.clone()));
        };
        let layers = self.meshes.entry(net).or_insert_with(|| {
            let copper = net_only(board, net);
            let tessellator = Tessellator::new(TOLERANCE);
            scene
                .stack
                .copper
                .iter()
                .filter_map(|name| {
                    let mesh = tessellator.copper(&copper, board.layer_named(name)?);
                    (!mesh.is_empty()).then(|| (Layer::Copper(name.clone()), mesh))
                })
                .collect()
        });
        let mut vertices = scene.vertices.clone();
        let mut indices = scene.indices.clone();
        let mut ranges = Vec::new();
        for (layer, mesh) in layers.iter() {
            let base = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
            let start = u32::try_from(indices.len()).unwrap_or(u32::MAX);
            vertices.extend(mesh.positions.iter().map(|position| Vertex {
                position: relative(*position, scene.origin),
            }));
            indices.extend(mesh.indices.iter().map(|index| index + base));
            let end = u32::try_from(indices.len()).unwrap_or(u32::MAX);
            ranges.push((layer.clone(), start..end));
        }
        self.uploaded = Some((net, ranges));
        Some((vertices, indices))
    }

    /// The draws of one frame: the scene's under `state`, each at
    /// [`DIM`] of its opacity when a net is highlighted (a page-coloured
    /// slice stays as it is), then the net's copper on each shown copper
    /// layer in the layer's colour, in the view's layer order. Without a
    /// highlight, or before [`Highlight::geometry`] gave the net's
    /// copper, the scene's draws as they are.
    #[must_use]
    pub fn draws(
        &self,
        scene: &Scene,
        state: &ViewState,
        format: wgpu::TextureFormat,
    ) -> Vec<Draw> {
        let mut draws = scene.draw_list(state, format);
        let (Some(net), Some((uploaded, ranges)), Some(view)) =
            (self.active(), self.uploaded.as_ref(), scene.view(state))
        else {
            return draws;
        };
        if *uploaded != net {
            return draws;
        }
        for draw in &mut draws {
            if !is_page(scene, draw) {
                draw.color[3] *= DIM;
            }
        }
        for layer in shown_copper(scene, state) {
            let Some((_, range)) = ranges.iter().find(|(on, _)| *on == layer) else {
                continue;
            };
            draws.push(Draw {
                range: range.clone(),
                color: view.color(&scene.stack, &layer).for_format(format),
            });
        }
        draws
    }
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

/// Look at a pad picked from the net's list: panned to its centre at the
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
    use std::sync::OnceLock;

    use kicad_core::hit::{find_footprint, footprint_bounds, net_pads};
    use kicad_core::model::LayerId;

    use super::*;
    use crate::camera::Size;

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
    fn a_pin_outlives_a_hover_and_escape_clears_both() {
        let (board, _) = hand_off();
        let mut highlight = Highlight::new(board);
        let (pad, via) = gnd_pad_and_vdd_via(board, &highlight);
        assert_eq!(highlight.active(), None);
        assert!(highlight.hover(Some(pad)));
        assert_eq!(highlight.active(), Some(pad.net));
        assert!(
            !highlight.hover(Some(pad)),
            "the same net again is no change"
        );
        assert!(highlight.pin(Some(via)));
        assert_eq!(highlight.active(), Some(via.net));
        assert!(
            !highlight.hover(Some(pad)),
            "a hover under a pin changes nothing"
        );
        assert_eq!(highlight.hovered(), Some(pad));
        assert_eq!(highlight.pinned(), Some(via.net));
        assert!(!highlight.hover(None));
        assert_eq!(
            highlight.active(),
            Some(via.net),
            "the pin survives the pointer leaving"
        );
        assert!(highlight.pin(None), "a click on nothing clears the pin");
        assert_eq!(highlight.active(), None);
        highlight.hover(Some(pad));
        assert!(highlight.clear());
        assert_eq!((highlight.active(), highlight.hovered()), (None, None));
        assert!(!highlight.clear(), "nothing to clear");
    }

    #[test]
    fn the_tooltip_names_the_item_and_its_net() {
        let (board, _) = hand_off();
        let mut highlight = Highlight::new(board);
        assert_eq!(highlight.tooltip(board), "");
        let (pad, via) = gnd_pad_and_vdd_via(board, &highlight);
        highlight.hover(Some(pad));
        assert_eq!(highlight.tooltip(board), "LED6 pad 3: GND");
        highlight.hover(Some(via));
        assert_eq!(highlight.tooltip(board), "via: Net-(LED5-VDD)");
        let track = highlight
            .index()
            .hit(Point { x: 110.0, y: 86.5 }, 0.0, &f_cu(board))
            .expect("the first segment in the file");
        highlight.hover(Some(track));
        assert!(highlight.tooltip(board).starts_with("track: "));
    }

    #[test]
    fn a_highlighted_net_dims_the_scene_and_adds_its_copper_on_the_shown_layers() {
        let (board, scene) = hand_off();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let state = ViewState::new(5);
        let mut highlight = Highlight::new(board);
        assert_eq!(
            highlight.geometry(board, scene),
            None,
            "the scene alone is right"
        );
        let plain = highlight.draws(scene, &state, format);
        assert_eq!(plain, scene.draw_list(&state, format));
        let (pad, _) = gnd_pad_and_vdd_via(board, &highlight);
        highlight.pin(Some(pad));
        let (vertices, indices) = highlight
            .geometry(board, scene)
            .expect("the net's copper is appended");
        assert!(vertices.len() > scene.vertices.len());
        assert!(indices.len() > scene.indices.len());
        assert_eq!(indices.len() % 3, 0);
        let max_index = indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < vertices.len());
        assert_eq!(highlight.geometry(board, scene), None, "uploaded once");
        let draws = highlight.draws(scene, &state, format);
        // View 5 shows one copper layer, F.Cu: one highlight draw after
        // the scene's, in the copper colour at full opacity.
        assert_eq!(draws.len(), plain.len() + 1);
        // GND is appended for every copper layer, F.Cu's run first: the
        // draw is a run past the scene's indices, inside the buffer.
        let last = draws.last().expect("the highlight draw");
        assert!(usize::try_from(last.range.start).expect("fits") >= scene.indices.len());
        assert!(usize::try_from(last.range.end).expect("fits") <= indices.len());
        assert!(last.range.end > last.range.start);
        let copper = crate::view::palette::COPPER.for_format(format);
        for (channel, expected) in last.color.iter().zip(copper) {
            assert!((channel - expected).abs() < 1e-6, "{last:?}");
        }
        for (dimmed, original) in draws.iter().zip(&plain) {
            assert_eq!(dimmed.range, original.range);
            let expected = if is_page(scene, original) {
                original.color[3]
            } else {
                original.color[3] * DIM
            };
            assert!((dimmed.color[3] - expected).abs() < 1e-6, "{dimmed:?}");
            assert_eq!(dimmed.color[..3], original.color[..3]);
        }
        // GND reaches every copper layer: the view with both outer
        // coppers gets two highlight draws.
        let both = highlight.draws(scene, &ViewState::new(7), format);
        let highlights = both
            .iter()
            .filter(|draw| usize::try_from(draw.range.start).expect("fits") >= scene.indices.len())
            .count();
        assert_eq!(highlights, 2);
        // Clearing hands the scene alone back once.
        highlight.clear();
        let (vertices, _) = highlight.geometry(board, scene).expect("the scene alone");
        assert_eq!(vertices.len(), scene.vertices.len());
        assert_eq!(highlight.geometry(board, scene), None);
        highlight.invalidate();
        assert_eq!(
            highlight.geometry(board, scene),
            None,
            "nothing active, nothing to give"
        );
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
