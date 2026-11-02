//! The net highlight on a sheet (docs/v1-scope.md, Net interaction; the
//! board's is `crate::highlight`, whose [`Pick`] keeps the hovered and
//! pinned state here too). The hit test is `kicad_core::schematic::hit`
//! and the nets `kicad_core::schematic::connect`; this module keeps the
//! parsed sheet with both, the geometry of each highlighted net
//! tessellated once and cached, and turns the sheet's draw list into the
//! dimmed one: every layer at [`DIM`] of its opacity, then the net's
//! wires, junctions, labels, pin lines and sheet pin flags on each of
//! their layers in the layer's own colour at full opacity. No layer is
//! re-tessellated: the dim is a paint change through the per-draw
//! colour, and the highlight is extra geometry appended after the
//! sheet's own, uploaded once per net change.
//!
//! The camera moves are the board's: a symbol found by its reference is
//! framed as a footprint is, a pin picked from the net's list is panned
//! to as a pad is.

use std::collections::BTreeMap;
use std::ops::Range;

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;
use kicad_core::schematic::connect::{Connectivity, NetPin, SheetItem, SheetNetId, net_pins};
use kicad_core::schematic::hit::{SheetHit, SheetIndex, find_symbol, symbol_bounds};
use kicad_core::schematic::model::Sheet;
use kicad_core::tessellate::schematic::SchematicLayer;
use kicad_core::tessellate::{Mesh, Tessellator};

use crate::camera::Camera;
use crate::highlight::{self, DIM, OnNet, Pick};
use crate::render::{Draw, Vertex};
use crate::scene::TOLERANCE;
use crate::schematic::{SheetScene, palette, relative};

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

/// Where a net's geometry on one layer sits in the index buffer after
/// the sheet's own indices.
type HighlightRange = (SchematicLayer, Range<u32>);

/// The highlight state of one sheet: the sheet itself, its nets, the hit
/// test over them, and what is hovered and pinned.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetHighlight {
    sheet: Sheet,
    connectivity: Connectivity,
    index: SheetIndex,
    pick: Pick<SheetHit>,
    /// Each net's geometry by layer, tessellated the first time it is
    /// highlighted.
    meshes: BTreeMap<SheetNetId, Vec<(SchematicLayer, Mesh)>>,
    /// The net whose geometry the GPU holds after the sheet's own, and
    /// where each layer's run of it sits; `None` when the GPU holds the
    /// sheet alone.
    uploaded: Option<(SheetNetId, Vec<HighlightRange>)>,
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
            pick: Pick::default(),
            meshes: BTreeMap::new(),
            uploaded: None,
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

    /// The net a frame highlights: the pinned one, else the hovered
    /// item's.
    #[must_use]
    pub fn active(&self) -> Option<SheetNetId> {
        self.pick.active()
    }

    /// What the pointer is over.
    #[must_use]
    pub fn hovered(&self) -> Option<SheetHit> {
        self.pick.hovered()
    }

    /// The net a click pinned.
    #[must_use]
    pub fn pinned(&self) -> Option<SheetNetId> {
        self.pick.pinned()
    }

    /// The pointer is over `hit` (or over nothing). Whether a frame is
    /// due.
    pub fn hover(&mut self, hit: Option<SheetHit>) -> bool {
        self.pick.hover(hit)
    }

    /// A click on `hit` pins its net; a click on nothing clears the pin.
    /// Whether a frame is due.
    pub fn pin(&mut self, hit: Option<SheetHit>) -> bool {
        self.pick.pin(hit)
    }

    /// Escape: nothing pinned, nothing hovered. Whether a frame is due.
    pub fn clear(&mut self) -> bool {
        self.pick.clear()
    }

    /// The GPU no longer holds the highlight's geometry (the sheet was
    /// uploaded afresh): the next [`SheetHighlight::geometry`] gives it
    /// again.
    pub fn invalidate(&mut self) {
        self.uploaded = None;
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
        let Some(hit) = self.hovered() else {
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

    /// The pins of the pinned net, in natural order; empty without a pin.
    #[must_use]
    pub fn pinned_pins(&self) -> Vec<NetPin> {
        self.pinned()
            .map(|net| net_pins(&self.sheet, &self.connectivity, net))
            .unwrap_or_default()
    }

    /// The geometry the GPU should hold for the next frame when it does
    /// not already: `scene`'s vertices and indices with the active net's
    /// geometry appended, positions relative to the scene's origin like
    /// the scene's own. `None` when what it holds is right.
    pub fn geometry(&mut self, scene: &SheetScene) -> Option<(Vec<Vertex>, Vec<u32>)> {
        let active = self.active();
        if self.uploaded.as_ref().map(|(net, _)| *net) == active {
            return None;
        }
        let Some(net) = active else {
            self.uploaded = None;
            return Some((scene.vertices.clone(), scene.indices.clone()));
        };
        let sheet = &self.sheet;
        let connectivity = &self.connectivity;
        let layers = self.meshes.entry(net).or_insert_with(|| {
            connectivity.net(net).map_or_else(Vec::new, |net| {
                Tessellator::new(TOLERANCE).sheet_net(sheet, net)
            })
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
            ranges.push((*layer, start..end));
        }
        self.uploaded = Some((net, ranges));
        Some((vertices, indices))
    }

    /// The draws of one frame: the sheet's, each at [`DIM`] of its
    /// opacity when a net is highlighted, then the net's geometry on each
    /// of its layers in the layer's colour. Without a highlight, or
    /// before [`SheetHighlight::geometry`] gave the net's geometry, the
    /// sheet's draws as they are.
    #[must_use]
    pub fn draws(&self, scene: &SheetScene, format: wgpu::TextureFormat) -> Vec<Draw> {
        let mut draws = scene.draws.clone();
        let (Some(net), Some((uploaded, ranges))) = (self.active(), self.uploaded.as_ref()) else {
            return draws;
        };
        if *uploaded != net {
            return draws;
        }
        for draw in &mut draws {
            draw.color[3] *= DIM;
        }
        for (layer, range) in ranges {
            draws.push(Draw {
                range: range.clone(),
                color: palette::of(*layer).for_format(format),
            });
        }
        draws
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

    /// Look at pin `index` of [`SheetHighlight::pinned_pins`], panned to
    /// as a pad is. False when there is no such pin.
    pub fn go_to_pin(&self, camera: &mut Camera, index: usize) -> bool {
        let Some(pin) = self.pinned_pins().into_iter().nth(index) else {
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

    use crate::camera::Size;

    use super::*;

    const ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kicad_core/tests/fixtures/hierarchy/hier.kicad_sch"
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
    fn a_pin_outlives_a_hover_and_escape_clears_both() {
        let (fixture, _) = root();
        let mut highlight = fixture.clone();
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        let ground = highlight.hit_at(at(50.0, 53.0), 0.0).expect("R1's pin 2");
        assert_ne!(wire.net, ground.net);
        assert_eq!(highlight.active(), None);
        assert!(highlight.hover(Some(wire)));
        assert_eq!(highlight.active(), Some(wire.net));
        assert!(highlight.pin(Some(ground)));
        assert!(
            !highlight.hover(Some(wire)),
            "a hover under a pin changes nothing"
        );
        assert!(!highlight.hover(None));
        assert_eq!(
            highlight.active(),
            Some(ground.net),
            "the pin survives the pointer leaving"
        );
        assert!(highlight.pin(None));
        assert_eq!(highlight.active(), None);
        highlight.hover(Some(wire));
        assert!(highlight.clear());
        assert_eq!((highlight.active(), highlight.hovered()), (None, None));
    }

    #[test]
    fn the_tooltip_names_the_item_and_its_net() {
        let (fixture, _) = root();
        let mut highlight = fixture.clone();
        assert_eq!(highlight.tooltip(), "");
        let hover = |highlight: &mut SheetHighlight, x: f64, y: f64| {
            let hit = highlight.hit_at(at(x, y), 0.0);
            highlight.hover(hit);
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
        hand_off.pin(Some(gnd));
        assert_eq!(hand_off.net_name(gnd.net), "GND");
        assert!(hand_off.pinned_pins().len() > 20);
    }

    #[test]
    fn a_highlighted_net_dims_the_sheet_and_adds_its_geometry_by_layer() {
        let (fixture, scene) = root();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut highlight = fixture.clone();
        assert_eq!(highlight.geometry(scene), None, "the sheet alone is right");
        assert_eq!(highlight.draws(scene, format), scene.draws);
        let wire = highlight.hit_at(at(60.0, 40.0), 0.0).expect("the VIN wire");
        highlight.pin(Some(wire));
        let (vertices, indices) = highlight
            .geometry(scene)
            .expect("the net's geometry is appended");
        assert!(vertices.len() > scene.vertices.len());
        assert!(indices.len() > scene.indices.len());
        assert_eq!(indices.len() % 3, 0);
        let max_index = indices.iter().max().copied().unwrap_or(0);
        assert!(usize::try_from(max_index).expect("fits") < vertices.len());
        assert_eq!(highlight.geometry(scene), None, "uploaded once");
        let draws = highlight.draws(scene, format);
        // VIN draws on five layers: wire, global label, junction, sheet
        // label, pin.
        assert_eq!(draws.len(), scene.draws.len() + 5);
        for (dimmed, original) in draws.iter().zip(&scene.draws) {
            assert_eq!(dimmed.range, original.range);
            assert!(
                (dimmed.color[3] - original.color[3] * DIM).abs() < 1e-6,
                "{dimmed:?}"
            );
            assert_eq!(dimmed.color[..3], original.color[..3]);
        }
        let last = draws.last().expect("the highlight draws");
        assert!(usize::try_from(last.range.start).expect("fits") >= scene.indices.len());
        assert!(usize::try_from(last.range.end).expect("fits") <= indices.len());
        let pin_colour = palette::of(SchematicLayer::Pin).for_format(format);
        for (channel, expected) in last.color.iter().zip(pin_colour) {
            assert!((channel - expected).abs() < 1e-6, "{last:?}");
        }
        assert!((last.color[3] - 1.0).abs() < 1e-6, "full opacity");
        highlight.clear();
        let (vertices, _) = highlight.geometry(scene).expect("the sheet alone");
        assert_eq!(vertices.len(), scene.vertices.len());
        highlight.invalidate();
        assert_eq!(
            highlight.geometry(scene),
            None,
            "nothing active, nothing to give"
        );
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
        highlight.pin(Some(wire));
        let pins = highlight.pinned_pins();
        assert_eq!(pins.len(), 2);
        assert!(highlight.go_to_pin(&mut camera, 1));
        assert!((camera.center().x - 50.0).abs() < 1e-6);
        assert!((camera.center().y - 46.19).abs() < 1e-6);
        assert!(
            camera.scale() > fitted,
            "a pin from the fitted view is zoomed to"
        );
        assert!(!highlight.go_to_pin(&mut camera, 2));
    }
}
