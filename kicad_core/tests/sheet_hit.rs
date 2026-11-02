//! Hit testing on a sheet (docs/v1-scope.md, Net interaction): what is
//! under a point on the hierarchy fixture's root and on the hand-off
//! sheet, the sheet symbol a click enters, the reference search, and the
//! net's geometry the highlight draws.

use std::sync::OnceLock;

use kicad_core::model::Point;
use kicad_core::parse_schematic;
use kicad_core::schematic::connect::{Connectivity, SheetItem};
use kicad_core::schematic::hit::{SheetIndex, SheetKind, find_symbol, symbol_bounds};
use kicad_core::schematic::model::Sheet;
use kicad_core::tessellate::Tessellator;
use kicad_core::tessellate::schematic::SchematicLayer;
use test_case::test_case;

const ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/hierarchy/hier.kicad_sch"
);
const HAND_OFF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
);

struct Indexed {
    sheet: Sheet,
    connectivity: Connectivity,
    index: SheetIndex,
}

fn indexed(path: &str) -> Indexed {
    let source = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let sheet = parse_schematic(&source).unwrap_or_else(|error| panic!("{path}: {error}"));
    let connectivity = Connectivity::build(&sheet);
    let index = SheetIndex::build(&sheet, &connectivity);
    Indexed {
        sheet,
        connectivity,
        index,
    }
}

fn root() -> &'static Indexed {
    static ROOT_SHEET: OnceLock<Indexed> = OnceLock::new();
    ROOT_SHEET.get_or_init(|| indexed(ROOT))
}

fn hand_off() -> &'static Indexed {
    static HAND_OFF_SHEET: OnceLock<Indexed> = OnceLock::new();
    HAND_OFF_SHEET.get_or_init(|| indexed(HAND_OFF))
}

fn at(x: f64, y: f64) -> Point {
    Point { x, y }
}

#[test_case((60.0, 40.0), 0.0, Some((SheetKind::Wire, "VIN")); "on a wire")]
#[test_case((60.0, 40.5), 0.0, None; "beside the wire with no tolerance")]
#[test_case((60.0, 40.5), 1.0, Some((SheetKind::Wire, "VIN")); "beside the wire within the tolerance")]
#[test_case((50.0, 40.0), 0.0, Some((SheetKind::Pin, "VIN")); "the power pin wins over the junction and the wires at the same point")]
#[test_case((50.0, 40.4), 0.0, Some((SheetKind::Junction, "VIN")); "the junction dot past the pin's end")]
#[test_case((50.0, 47.0), 0.0, Some((SheetKind::Pin, "VIN")); "on R1's pin 1 line")]
#[test_case((50.0, 53.0), 0.0, Some((SheetKind::Pin, "GND")); "on R1's pin 2 line")]
#[test_case((72.0, 40.0), 0.0, Some((SheetKind::Label, "VIN")); "inside the global label's flag")]
#[test_case((62.0, 79.0), 0.0, Some((SheetKind::Label, "SIG")); "over a local label's text")]
#[test_case((80.6, 40.0), 0.0, Some((SheetKind::SheetPin, "VIN")); "inside a sheet pin's flag")]
#[test_case((25.0, 100.0), 0.0, Some((SheetKind::Wire, "Net-(R4-Pad1)")); "a wire on an unnamed net")]
#[test_case((95.0, 40.0), 0.0, None; "inside a sheet symbol's box is no net")]
#[test_case((150.0, 150.0), 5.0, None; "empty space")]
#[test_case((f64::NAN, 40.0), 0.0, None; "a point that is not a number hits nothing")]
fn the_nearest_item_of_the_best_kind_is_hit(
    point: (f64, f64),
    tolerance: f64,
    expected: Option<(SheetKind, &str)>,
) {
    let Indexed {
        connectivity,
        index,
        ..
    } = root();
    let hit = index.hit(at(point.0, point.1), tolerance);
    let found = hit.map(|hit| {
        (
            hit.item.kind(),
            connectivity
                .net(hit.net)
                .map_or("", |net| net.name.as_str()),
        )
    });
    assert_eq!(found, expected, "{hit:?}");
}

#[test]
fn a_hit_names_the_item_in_the_model() {
    let Indexed { index, .. } = root();
    assert_eq!(
        index.hit(at(60.0, 40.0), 0.0).map(|hit| hit.item),
        Some(SheetItem::Wire(2))
    );
    assert_eq!(
        index.hit(at(50.0, 47.0), 0.0).map(|hit| hit.item),
        Some(SheetItem::Pin {
            symbol: 0,
            unit: 1,
            pin: 0
        })
    );
    assert_eq!(
        index.hit(at(80.6, 40.0), 0.0).map(|hit| hit.item),
        Some(SheetItem::SheetPin { sheet: 0, pin: 0 })
    );
    // Six wires, one junction, two labels, four connecting pins (R1's
    // two, R4's two, and the two power pins), two sheet pins.
    assert_eq!(index.len(), 6 + 1 + 2 + 6 + 2);
    assert!(!index.is_empty());
    assert!(SheetIndex::default().is_empty());
}

#[test_case((95.0, 40.0), Some(0); "inside power")]
#[test_case((80.0, 30.0), Some(0); "on its corner")]
#[test_case((95.0, 80.0), Some(1); "inside io")]
#[test_case((60.0, 40.0), None; "on a wire outside every sheet")]
fn a_click_inside_a_sheet_symbol_names_it(point: (f64, f64), expected: Option<usize>) {
    let Indexed { index, .. } = root();
    assert_eq!(index.sheet_symbol_at(at(point.0, point.1)), expected);
}

#[test_case("r1", Some("R1"); "lower case finds it")]
#[test_case(" R4 ", Some("R4"); "spaces around are ignored")]
#[test_case("#PWR02", Some("#PWR02"); "a power symbol by its hidden reference")]
#[test_case("R9", None; "a reference the sheet lacks")]
#[test_case("", None; "nothing asked")]
fn a_reference_is_found_whatever_its_case(query: &str, found: Option<&str>) {
    let Indexed { sheet, .. } = root();
    let index = find_symbol(sheet, query);
    assert_eq!(index.and_then(|i| sheet.symbols[i].reference()), found);
}

#[test]
fn a_symbols_box_holds_its_body_and_its_pins() {
    let Indexed { sheet, .. } = root();
    // R1 at (50, 50), unturned: the body is 2.032 wide and 5.08 tall, the
    // pins reach 3.81 above and below.
    let r1 = symbol_bounds(sheet, &sheet.symbols[0]).expect("a box");
    assert!((r1.min.x - 48.984).abs() < 1e-6, "{r1:?}");
    assert!((r1.max.x - 51.016).abs() < 1e-6, "{r1:?}");
    assert!((r1.min.y - 46.19).abs() < 1e-6, "{r1:?}");
    assert!((r1.max.y - 53.81).abs() < 1e-6, "{r1:?}");
    // A power symbol's only pin is hidden: the box is its graphics.
    let gnd = symbol_bounds(sheet, &sheet.symbols[3]).expect("a box");
    assert!(
        (gnd.min.y - 60.0).abs() < 1e-6 && (gnd.max.y - 62.54).abs() < 1e-6,
        "{gnd:?}"
    );
    let Indexed { sheet, .. } = hand_off();
    let r5 = find_symbol(sheet, "R5").expect("R5");
    let r5 = symbol_bounds(sheet, &sheet.symbols[r5]).expect("a box");
    assert!(r5.min.y <= 138.43 && r5.max.y >= 143.51, "{r5:?}");
    assert!(r5.width() < 3.0, "{r5:?}");
}

#[test]
fn the_hand_off_sheets_labels_pins_and_wires_are_where_the_file_puts_them() {
    let Indexed {
        sheet,
        connectivity,
        index,
    } = hand_off();
    let net_name = |hit: Option<kicad_core::schematic::hit::SheetHit>| {
        hit.and_then(|hit| connectivity.net(hit.net))
            .map(|net| net.name.as_str())
    };
    // The SWO global label at (33.02, 104.14) reads to the right: its
    // flag spans the next few millimetres.
    let swo = index.hit(at(35.0, 104.14), 0.0);
    assert_eq!(swo.map(|hit| hit.item.kind()), Some(SheetKind::Label));
    assert_eq!(net_name(swo), Some("SWO"));
    // R5's pin 1 line runs from (196.85, 138.43) towards the body.
    let pin = index.hit(at(196.85, 138.9), 0.0);
    assert_eq!(pin.map(|hit| hit.item.kind()), Some(SheetKind::Pin));
    // The long wire along y 138.43.
    let wire = index.hit(at(150.0, 138.43), 0.0);
    assert_eq!(wire.map(|hit| hit.item.kind()), Some(SheetKind::Wire));
    assert_eq!(net_name(wire), net_name(pin), "the wire ends on the pin");
    assert_eq!(index.hit(at(5.0, 5.0), 0.5), None, "the margin is empty");
    let connecting: usize = connectivity.nets().iter().map(|net| net.items.len()).sum();
    assert_eq!(index.len(), connecting, "every item on a net is indexed");
    assert!(index.len() > sheet.wires.len() + sheet.labels.len());
}

#[test]
fn a_nets_geometry_is_its_wires_junctions_labels_and_pins_by_layer() {
    let Indexed {
        sheet,
        connectivity,
        ..
    } = root();
    let vin = connectivity
        .net_of(SheetItem::Label(1))
        .and_then(|id| connectivity.net(id))
        .expect("VIN");
    let layers = Tessellator::new(0.01).sheet_net(sheet, vin);
    let names: Vec<SchematicLayer> = layers.iter().map(|(layer, _)| *layer).collect();
    assert_eq!(
        names,
        [
            SchematicLayer::Wire,
            SchematicLayer::GlobalLabel,
            SchematicLayer::Junction,
            SchematicLayer::SheetLabel,
            SchematicLayer::Pin,
        ],
        "in draw order, with no pin texts and no symbol bodies"
    );
    let bounds = layers
        .iter()
        .filter_map(|(_, mesh)| mesh.bounds())
        .reduce(kicad_core::geometry::Bounds::union)
        .expect("geometry");
    // From the junction at (50, 40) down R1's pin to 47.46 and right to
    // the sheet pin's flag at 80 and its text inside the sheet.
    assert!(bounds.min.x > 48.0 && bounds.min.x < 50.0, "{bounds:?}");
    assert!(bounds.max.x > 81.0 && bounds.max.x < 86.0, "{bounds:?}");
    assert!(bounds.min.y > 37.0 && bounds.min.y < 40.0, "{bounds:?}");
    assert!(bounds.max.y > 47.0 && bounds.max.y < 48.0, "{bounds:?}");
    // GND on the root: a wire, a hidden power pin that draws nothing,
    // and R1's pin 2.
    let gnd = connectivity
        .net_of(SheetItem::Wire(1))
        .and_then(|id| connectivity.net(id))
        .expect("GND");
    let layers = Tessellator::new(0.01).sheet_net(sheet, gnd);
    let names: Vec<SchematicLayer> = layers.iter().map(|(layer, _)| *layer).collect();
    assert_eq!(names, [SchematicLayer::Wire, SchematicLayer::Pin]);
}
