//! The net highlight on a sheet (docs/v1-scope.md, Net interaction; the
//! board's is `crate::highlight`). The hit test is
//! `kicad_core::schematic::hit` and the nets
//! `kicad_core::schematic::connect`; this module keeps the parsed sheet
//! with both, the geometry of each lit net tessellated once and cached,
//! and turns the sheet's draw list into the dimmed one: every layer at
//! the pick's dim of its opacity, then each lit net's wires, junctions,
//! labels, pin lines and sheet pin flags on each of their layers, in the
//! net's slot's colour. Slot 0 keeps each item's own colour. A sheet
//! shares the board's [`Pick`] gestures, undo and colour slots, in its
//! flat form: hover dims like a pin, and nothing eases. No layer is
//! re-tessellated: the dim is a paint change through the per-draw
//! colour, and the lit nets are extra geometry after the sheet's own, in
//! the board's [`Tail`].
//!
//! The camera moves are the board's: a symbol found by its reference is
//! framed as a footprint is, a pin picked from a net's list is panned
//! to as a pad is.

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;
use kicad_core::schematic::connect::{Connectivity, NetPin, SheetItem, SheetNetId, net_pins};
use kicad_core::schematic::hit::{SheetHit, SheetIndex, find_symbol, symbol_bounds};
use kicad_core::schematic::model::Sheet;
use kicad_core::tessellate::Tessellator;
use kicad_core::tessellate::schematic::SchematicLayer;

use crate::camera::Camera;
use crate::highlight::{self, Ramp, Tail};
use crate::pick::{OnNet, Pick};
use crate::render::{Draw, Rgb, Splice};
use crate::scene::TOLERANCE;
use crate::schematic::{SheetScene, palette};

impl OnNet for SheetHit {
    type Net = SheetNetId;

    fn net(self) -> SheetNetId {
        self.net
    }
}

/// Half the side of the box a pin is panned to, in millimetres: a pin's
/// connection point with a little room, so the zoom rule has an extent
/// to measure.
const PIN_HALF: f64 = 1.27;

/// The highlight state of one sheet: the sheet itself, its nets, the hit
/// test over them, and what is hovered and pinned.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetHighlight {
    sheet: Sheet,
    connectivity: Connectivity,
    index: SheetIndex,
    pick: Pick<SheetHit>,
    /// Each lit net's geometry by layer.
    tail: Tail<SheetNetId, SchematicLayer>,
}

impl SheetHighlight {
    /// The highlight over `sheet`, nothing hovered or pinned.
    #[must_use]
    pub fn new(sheet: Sheet) -> Self {
        let connectivity = Connectivity::build(&sheet);
        let index = SheetIndex::build(&sheet, &connectivity);
        Self {
            sheet,
            connectivity,
            index,
            pick: Pick::flat(),
            tail: Tail::default(),
        }
    }

    /// The sheet as parsed.
    #[must_use]
    pub fn sheet(&self) -> &Sheet {
        &self.sheet
    }

    /// The sheet's nets.
    #[must_use]
    pub fn connectivity(&self) -> &Connectivity {
        &self.connectivity
    }

    /// The hit test over the sheet.
    #[must_use]
    pub fn index(&self) -> &SheetIndex {
        &self.index
    }

    /// What is hovered and pinned, and what a frame lights.
    #[must_use]
    pub fn pick(&self) -> &Pick<SheetHit> {
        &self.pick
    }

    /// The same, to change.
    pub fn pick_mut(&mut self) -> &mut Pick<SheetHit> {
        &mut self.pick
    }

    /// The GPU no longer holds the highlight's geometry (the sheet was
    /// uploaded afresh): the next [`SheetHighlight::geometry`] gives it
    /// again.
    pub fn invalidate(&mut self) {
        self.tail.invalidate();
    }

    /// The name of net `id`; empty for one the sheet lacks.
    #[must_use]
    pub fn net_name(&self, id: SheetNetId) -> &str {
        self.connectivity
            .net(id)
            .map_or("", |net| net.name.as_str())
    }

    /// What the hovered item is, for the tooltip: `wire: VIN`,
    /// `junction: VIN`, `label: VIN`, `R1 pin 2: GND` (`U2 pin 5 VBUS:
    /// VBUS` when the pin has a name), `sheet pin VIN: VIN`; empty over
    /// nothing.
    #[must_use]
    pub fn tooltip(&self) -> String {
        let Some(hit) = self.pick.hovered() else {
            return String::new();
        };
        let what = match hit.item {
            SheetItem::Wire(_) => "wire".to_owned(),
            SheetItem::Junction(_) => "junction".to_owned(),
            SheetItem::Label(_) => "label".to_owned(),
            SheetItem::Pin { symbol, unit, pin } => {
                let placed = self.sheet.symbols.get(symbol);
                let reference = placed.and_then(|s| s.reference()).unwrap_or("?");
                let pin = placed
                    .and_then(|placed| self.sheet.library_symbol(placed))
                    .and_then(|library| library.units.get(unit))
                    .and_then(|unit| unit.pins.get(pin));
                match pin {
                    Some(pin) if !pin.shown_name().is_empty() => {
                        format!("{reference} pin {} {}", pin.number, pin.shown_name())
                    }
                    Some(pin) => format!("{reference} pin {}", pin.number),
                    None => format!("{reference} pin"),
                }
            }
            SheetItem::SheetPin { sheet, pin } => {
                let name = self
                    .sheet
                    .sheet_symbols
                    .get(sheet)
                    .and_then(|symbol| symbol.pins.get(pin))
                    .map_or("", |pin| pin.text.text.as_str());
                format!("sheet pin {name}")
            }
        };
        format!("{what}: {}", self.net_name(hit.net))
    }

    /// The pins of the net in row `row` of the net block, in natural
    /// order; empty past the last row.
    #[must_use]
    pub fn pinned_pins(&self, row: usize) -> Vec<NetPin> {
        self.pick
            .pins()
            .get(row)
            .map(|pinned| net_pins(&self.sheet, &self.connectivity, pinned.net))
            .unwrap_or_default()
    }

    /// What the GPU should take for the next frame to hold the lit
    /// nets' geometry after `scene`'s; `None` when what it holds is
    /// right.
    pub fn geometry(&mut self, scene: &SheetScene) -> Option<Splice> {
        let nets: Vec<SheetNetId> = self.pick.lit().iter().map(|lit| lit.net).collect();
        for &net in &nets {
            let (sheet, connectivity) = (&self.sheet, &self.connectivity);
            self.tail.cache(net, || {
                connectivity.net(net).map_or_else(Vec::new, |net| {
                    Tessellator::new(TOLERANCE).sheet_net(sheet, net)
                })
            });
        }
        self.tail.sync(
            &nets,
            (scene.vertices.len(), scene.indices.len()),
            scene.origin,
        )
    }

    /// The draws of one frame: the sheet's at the pick's dim of their
    /// opacity, then each lit net in pick order on each of its layers:
    /// slot 0 in each layer's colour, a later slot in its own. A lit net
    /// [`SheetHighlight::geometry`] has not given yet draws nothing.
    #[must_use]
    pub fn draws(&self, scene: &SheetScene, format: wgpu::TextureFormat) -> Vec<Draw> {
        let mut draws = scene.draws.clone();
        for draw in &mut draws {
            draw.color[3] *= self.pick.dim();
        }
        // Each lit run a group of its own, nearer than the sheet's.
        let mut group = draws.iter().map(|draw| draw.group + 1).max().unwrap_or(0);
        for lit in self.pick.lit() {
            let generated = Ramp::SHEET.color(lit.slot);
            for (layer, range) in self.tail.runs(lit.net).unwrap_or_default() {
                let mut color = generated
                    .unwrap_or_else(|| palette::of(*layer))
                    .for_format(format);
                color[3] = lit.strength();
                draws.push(Draw {
                    range: range.clone(),
                    color,
                    group,
                });
                group += 1;
            }
        }
        draws
    }

    /// The colour of `slot`'s swatch: a generated slot's own, and a
    /// wire's for slot 0, which keeps each item's colour.
    #[must_use]
    pub fn swatch(slot: usize) -> Rgb {
        Ramp::SHEET
            .color(slot)
            .unwrap_or_else(|| palette::of(SchematicLayer::Wire))
    }

    /// The item under the world point `world`, within `tolerance`
    /// millimetres.
    #[must_use]
    pub fn hit_at(&self, world: Point, tolerance: f64) -> Option<SheetHit> {
        self.index.hit(world, tolerance)
    }

    /// Look at the symbol whose reference designator is `reference`,
    /// letter case aside, framed as a footprint is; one with neither body
    /// nor pins is framed as a millimetre around its origin. False, with
    /// the camera untouched, when the sheet has no such symbol.
    pub fn search_reference(&self, camera: &mut Camera, reference: &str) -> bool {
        let Some(symbol) =
            find_symbol(&self.sheet, reference).and_then(|index| self.sheet.symbols.get(index))
        else {
            return false;
        };
        let bounds = symbol_bounds(&self.sheet, symbol).unwrap_or_else(|| around(symbol.at, 1.0));
        highlight::go_to_footprint(camera, bounds);
        true
    }

    /// Look at pin `index` of [`SheetHighlight::pinned_pins`] of `row`,
    /// panned to as a pad is. False when there is no such pin.
    pub fn go_to_pin(&self, camera: &mut Camera, row: usize, index: usize) -> bool {
        let Some(pin) = self.pinned_pins(row).into_iter().nth(index) else {
            return false;
        };
        highlight::go_to_pad(camera, around(pin.at, PIN_HALF));
        true
    }
}

/// The box `half` millimetres each way around `center`.
fn around(center: Point, half: f64) -> Bounds {
    Bounds {
        min: Point {
            x: center.x - half,
            y: center.y - half,
        },
        max: Point {
            x: center.x + half,
            y: center.y + half,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use kiewer_assert::assert_is_not_empty;

    use crate::camera::Size;
    use crate::pick::Gesture;

    use super::*;

    const ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kicad_core/tests/fixtures/sheet_tree/hier.kicad_sch"
    );
    const HAND_OFF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_sch"
    );

    fn parsed(path: &str) -> Sheet {
        let source = std::fs::read_to_string(path).expect("the sheet is in the repo");
        kicad_core::parse_schematic(&source).expect("the sheet parses")
    }

    fn root() -> &'static (SheetHighlight, SheetScene) {
        static ROOT_SHEET: OnceLock<(SheetHighlight, SheetScene)> = OnceLock::new();
        ROOT_SHEET.get_or_init(|| {
            let sheet = parsed(ROOT);
            let scene = crate::schematic::build(&sheet, wgpu::TextureFormat::Rgba8Unorm)
                .expect("a sheet with content");
            (SheetHighlight::new(sheet), scene)
        })
    }

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    #[test]
    fn a_pin_outlives_a_hover_and_the_dim_stays_at_30_without_a_fade() {
        let (fixture, _) = root();
        let mut highlight = fixture.clone();
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        let ground = highlight.hit_at(at(50.0, 53.0), 0.0).expect("R1's pin 2");
        assert_ne!(wire.net, ground.net);
        assert!(highlight.pick_mut().hover(Some(wire)));
        assert!((highlight.pick().dim() - 0.3).abs() < 1e-6);
        assert!(!highlight.pick().fading());
        highlight.pick_mut().hover(None);
        assert!((highlight.pick().dim() - 1.0).abs() < 1e-6);
        assert!(highlight.pick_mut().press(Gesture::Click, Some(ground)));
        assert!(!highlight.pick_mut().hover(None), "the pin stands");
        assert_eq!(highlight.pick().pins().len(), 1);
    }

    #[test]
    fn the_tooltip_names_the_item_and_its_net() {
        let (fixture, _) = root();
        let mut highlight = fixture.clone();
        assert_eq!(highlight.tooltip(), "");
        let hover = |highlight: &mut SheetHighlight, x: f64, y: f64| {
            let hit = highlight.hit_at(at(x, y), 0.0);
            highlight.pick_mut().hover(hit);
            highlight.tooltip()
        };
        assert_eq!(hover(&mut highlight, 60.0, 40.0), "wire: VIN");
        assert_eq!(hover(&mut highlight, 50.0, 40.4), "junction: VIN");
        assert_eq!(hover(&mut highlight, 72.0, 40.0), "label: VIN");
        assert_eq!(hover(&mut highlight, 50.0, 53.0), "R1 pin 2: GND");
        assert_eq!(hover(&mut highlight, 80.6, 40.0), "sheet pin VIN: VIN");
        assert_eq!(hover(&mut highlight, 150.0, 150.0), "");
        let mut hand_off = SheetHighlight::new(parsed(HAND_OFF));
        assert_eq!(hover(&mut hand_off, 35.0, 104.14), "label: SWO");
        let gnd = hand_off
            .hit_at(at(92.71, 160.0), 0.0)
            .expect("the ground wire");
        hand_off.pick_mut().press(Gesture::Click, Some(gnd));
        assert_eq!(hand_off.net_name(gnd.net), "GND");
        assert!(hand_off.pinned_pins(0).len() > 20);
    }

    #[test]
    fn a_pin_dims_the_sheet_and_adds_its_geometry_by_layer() {
        let (fixture, scene) = root();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut highlight = fixture.clone();
        assert_eq!(highlight.geometry(scene), None, "the sheet alone is right");
        assert_eq!(highlight.draws(scene, format), scene.draws);
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        highlight.pick_mut().press(Gesture::Click, Some(wire));
        let splice = highlight
            .geometry(scene)
            .expect("the net's geometry is appended");
        assert_eq!(
            usize::try_from(splice.keep_vertices).expect("fits"),
            scene.vertices.len()
        );
        assert_is_not_empty!(splice.vertices);
        assert_eq!(splice.indices.len() % 3, 0);
        let total = scene.vertices.len() + splice.vertices.len();
        let max_index = splice.indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < total);
        assert_eq!(highlight.geometry(scene), None, "uploaded once");
        let draws = highlight.draws(scene, format);
        // VIN draws on five layers: wire, global label, junction, sheet
        // label, pin.
        assert_eq!(draws.len(), scene.draws.len() + 5);
        for (dimmed, original) in draws.iter().zip(&scene.draws) {
            assert_eq!(dimmed.range, original.range);
            assert!(
                (dimmed.color[3] - original.color[3] * 0.3).abs() < 1e-6,
                "{dimmed:?}"
            );
            assert_eq!(dimmed.color[..3], original.color[..3]);
        }
        let last = draws.last().expect("the highlight draws");
        assert!(usize::try_from(last.range.start).expect("fits") >= scene.indices.len());
        let pin_colour = palette::of(SchematicLayer::Pin).for_format(format);
        for (channel, expected) in last.color.iter().zip(pin_colour) {
            assert!((channel - expected).abs() < 1e-6, "{last:?}");
        }
        highlight.pick_mut().clear();
        assert_eq!(highlight.geometry(scene), None, "only a shorter tail");
        assert_eq!(highlight.draws(scene, format), scene.draws);
    }

    #[test]
    fn a_later_pin_draws_every_layer_in_its_generated_colour() {
        let (fixture, scene) = root();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut highlight = fixture.clone();
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        let ground = highlight.hit_at(at(50.0, 53.0), 0.0).expect("R1's pin 2");
        highlight
            .pick_mut()
            .press(Gesture::ModifiedClick, Some(ground));
        highlight
            .pick_mut()
            .press(Gesture::ModifiedClick, Some(wire));
        highlight.geometry(scene);
        let draws = highlight.draws(scene, format);
        let last = draws.last().expect("the second pin's draws");
        let generated = Ramp::SHEET.color(1).expect("slot 1").for_format(format);
        assert_eq!(last.color[..3], generated[..3]);
    }

    #[test]
    fn a_hover_with_a_pin_previews_at_once_in_the_next_colour() {
        let (fixture, scene) = root();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut highlight = fixture.clone();
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        let ground = highlight.hit_at(at(50.0, 53.0), 0.0).expect("R1's pin 2");
        highlight.pick_mut().press(Gesture::Click, Some(ground));
        highlight.pick_mut().hover(Some(wire));
        highlight.geometry(scene);
        let draws = highlight.draws(scene, format);
        let last = draws.last().expect("the preview's draws");
        let generated = Ramp::SHEET.color(1).expect("slot 1").for_format(format);
        assert_eq!(last.color[..3], generated[..3]);
        assert!((last.color[3] - 1.0).abs() < 1e-6, "solid, with no fade");
    }

    #[test]
    fn a_symbol_is_framed_and_a_pin_is_panned_to() {
        let (fixture, scene) = root();
        let mut highlight = fixture.clone();
        let viewport = Size {
            width: 1600.0,
            height: 900.0,
        };
        let mut camera = Camera::new(viewport);
        camera.fit(scene.bounds);
        let fitted = camera.scale();
        assert!(highlight.search_reference(&mut camera, "r1"));
        assert!(
            (camera.center().x - 50.0).abs() < 1e-6,
            "{:?}",
            camera.center()
        );
        assert!(
            (camera.center().y - 50.0).abs() < 1e-6,
            "{:?}",
            camera.center()
        );
        assert!(camera.scale() > fitted);
        assert!(!highlight.search_reference(&mut camera, "R9"));
        camera.fit(scene.bounds);
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        highlight.pick_mut().press(Gesture::Click, Some(wire));
        let pins = highlight.pinned_pins(0);
        assert_eq!(pins.len(), 2);
        assert!(highlight.go_to_pin(&mut camera, 0, 1));
        assert!((camera.center().x - 50.0).abs() < 1e-6);
        assert!((camera.center().y - 46.19).abs() < 1e-6);
        assert!(
            camera.scale() > fitted,
            "a pin from the fitted view is zoomed to"
        );
        assert!(!highlight.go_to_pin(&mut camera, 0, 2));
        assert!(!highlight.go_to_pin(&mut camera, 1, 0));
    }
}
