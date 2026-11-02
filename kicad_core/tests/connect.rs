//! Net membership on a sheet (docs/v1-scope.md, Net interaction): which
//! wires, junctions, labels, pins and sheet pins belong together, and
//! what the net is called, on the hierarchy fixture and the hand-off
//! sheet.

use std::sync::OnceLock;

use kicad_core::model::Point;
use kicad_core::parse_schematic;
use kicad_core::schematic::connect::{Connectivity, SheetItem, net_pins};
use kicad_core::schematic::model::Sheet;
use test_case::test_case;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hierarchy");
const HAND_OFF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
);

fn parsed(path: &str) -> Sheet {
    let source = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
    parse_schematic(&source).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn root() -> &'static (Sheet, Connectivity) {
    static ROOT: OnceLock<(Sheet, Connectivity)> = OnceLock::new();
    ROOT.get_or_init(|| {
        let sheet = parsed(&format!("{FIXTURES}/hier.kicad_sch"));
        let connectivity = Connectivity::build(&sheet);
        (sheet, connectivity)
    })
}

fn hand_off() -> &'static (Sheet, Connectivity) {
    static HAND_OFF_SHEET: OnceLock<(Sheet, Connectivity)> = OnceLock::new();
    HAND_OFF_SHEET.get_or_init(|| {
        let sheet = parsed(HAND_OFF);
        let connectivity = Connectivity::build(&sheet);
        (sheet, connectivity)
    })
}

fn name_of(connectivity: &Connectivity, item: SheetItem) -> Option<&str> {
    connectivity
        .net_of(item)
        .and_then(|id| connectivity.net(id))
        .map(|net| net.name.as_str())
}

/// R1's pin `pin` (0 or 1) on the fixture's root: symbol 0, its one
/// unit with pins is index 1 of the library symbol.
fn r1_pin(pin: usize) -> SheetItem {
    SheetItem::Pin {
        symbol: 0,
        unit: 1,
        pin,
    }
}

#[test_case(SheetItem::Wire(0), Some("VIN"); "the wire from the junction to R1")]
#[test_case(SheetItem::Wire(2), Some("VIN"); "the wire from the junction to the label")]
#[test_case(SheetItem::Wire(3), Some("VIN"); "the wire from the label to the sheet pin")]
#[test_case(SheetItem::Junction(0), Some("VIN"); "the junction")]
#[test_case(SheetItem::Label(1), Some("VIN"); "the global label names the net over the power symbol")]
#[test_case(SheetItem::Pin { symbol: 2, unit: 1, pin: 0 }, Some("VIN"); "the hidden pin of a power symbol joins")]
#[test_case(SheetItem::SheetPin { sheet: 0, pin: 0 }, Some("VIN"); "the sheet pin")]
#[test_case(SheetItem::Wire(1), Some("GND"); "the wire into the ground symbol, named by the power pin")]
#[test_case(SheetItem::Pin { symbol: 3, unit: 1, pin: 0 }, Some("GND"); "the ground pin")]
#[test_case(SheetItem::Wire(4), Some("SIG"); "a wire under a local label")]
#[test_case(SheetItem::SheetPin { sheet: 1, pin: 0 }, Some("SIG"); "the sheet pin at the wire's end")]
#[test_case(SheetItem::Wire(5), Some("Net-(R4-Pad1)"); "a wire with only a pin takes KiCad's pin name")]
#[test_case(SheetItem::Pin { symbol: 1, unit: 1, pin: 1 }, Some("unconnected-(R4-Pad2)"); "a pin on its own")]
#[test_case(SheetItem::Wire(9), None; "a wire the sheet lacks")]
#[test_case(SheetItem::Pin { symbol: 0, unit: 0, pin: 0 }, None; "a unit with no pins")]
fn every_item_of_the_root_is_on_the_net_kicad_gives_it(item: SheetItem, net: Option<&str>) {
    let (_, connectivity) = root();
    assert_eq!(name_of(connectivity, item), net);
}

#[test]
fn r1_sits_between_vin_and_ground_and_the_root_has_five_nets() {
    let (sheet, connectivity) = root();
    assert_eq!(name_of(connectivity, r1_pin(0)), Some("VIN"));
    assert_eq!(name_of(connectivity, r1_pin(1)), Some("GND"));
    let names: Vec<&str> = connectivity
        .nets()
        .iter()
        .map(|net| net.name.as_str())
        .collect();
    assert_eq!(names.len(), 5, "{names:?}");
    for name in [
        "VIN",
        "GND",
        "SIG",
        "Net-(R4-Pad1)",
        "unconnected-(R4-Pad2)",
    ] {
        assert!(names.contains(&name), "{name} among {names:?}");
    }
    let vin = connectivity.net_of(SheetItem::Label(1)).expect("VIN");
    let vin_net = connectivity.net(vin).expect("VIN");
    assert_eq!(vin_net.items.len(), 8, "{:?}", vin_net.items);
    // The pins of VIN, in natural order with their connection points:
    // the power symbol's and R1's.
    let pins = net_pins(sheet, connectivity, vin);
    let listed: Vec<(&str, &str)> = pins
        .iter()
        .map(|pin| (pin.reference.as_str(), pin.number.as_str()))
        .collect();
    assert_eq!(listed, [("#PWR01", "1"), ("R1", "1")]);
    let r1 = pins.last().expect("R1's pin");
    assert!(
        (r1.at.x - 50.0).abs() < 1e-9 && (r1.at.y - 46.19).abs() < 1e-9,
        "{:?}",
        r1.at
    );
    assert_eq!(r1.item, r1_pin(0));
}

#[test]
fn the_child_sheets_name_their_nets_by_hierarchical_and_global_labels() {
    let power = parsed(&format!("{FIXTURES}/power.kicad_sch"));
    let connectivity = Connectivity::build(&power);
    assert_eq!(name_of(&connectivity, SheetItem::Wire(0)), Some("VIN"));
    assert_eq!(name_of(&connectivity, SheetItem::Wire(1)), Some("GND"));
    assert_eq!(
        name_of(&connectivity, SheetItem::SheetPin { sheet: 0, pin: 0 }),
        Some("SIG"),
        "a sheet pin on its own names its net"
    );
    let io = parsed(&format!("{FIXTURES}/sub/io.kicad_sch"));
    let connectivity = Connectivity::build(&io);
    assert_eq!(name_of(&connectivity, SheetItem::Label(0)), Some("SIG"));
    assert_eq!(
        name_of(
            &connectivity,
            SheetItem::Pin {
                symbol: 0,
                unit: 1,
                pin: 0
            }
        ),
        Some("SIG"),
        "a turned symbol's pin lands on the wire"
    );
    assert_eq!(name_of(&connectivity, SheetItem::Wire(1)), Some("SIG_OUT"));
    assert_eq!(connectivity.nets().len(), 2);
}

#[test]
fn a_wire_ending_on_another_wire_joins_it_and_a_crossing_does_not() {
    let source = r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols)
      (wire (pts (xy 0 10) (xy 20 10)) (stroke (width 0) (type default)) (uuid "a"))
      (wire (pts (xy 10 10) (xy 10 20)) (stroke (width 0) (type default)) (uuid "b"))
      (wire (pts (xy 5 0) (xy 5 20)) (stroke (width 0) (type default)) (uuid "c"))
      (wire (pts (xy 30 0) (xy 30 20)) (stroke (width 0) (type default)) (uuid "d"))
      (wire (pts (xy 25 5) (xy 35 5)) (stroke (width 0) (type default)) (uuid "e"))
      (junction (at 30 5) (diameter 0) (color 0 0 0 0) (uuid "j")))"#;
    let sheet = parse_schematic(source).expect("parses");
    let connectivity = Connectivity::build(&sheet);
    let net = |index| connectivity.net_of(SheetItem::Wire(index));
    assert_eq!(net(0), net(1), "b ends on a's middle");
    assert_ne!(net(0), net(2), "c crosses a without ending on it");
    assert_eq!(net(3), net(4), "a junction joins the crossing");
    assert_eq!(connectivity.net_of(SheetItem::Junction(0)), net(3));
    assert_eq!(connectivity.nets().len(), 3);
}

#[test]
fn the_hand_off_sheet_connects_as_kicad_drew_it() {
    let (sheet, connectivity) = hand_off();
    // The wire out of the ground symbol at (92.71, 161.29).
    let into_ground = sheet
        .wires
        .iter()
        .position(|wire| {
            wire.points
                .iter()
                .any(|p| (p.x - 92.71).abs() < 1e-6 && (p.y - 161.29).abs() < 1e-6)
        })
        .expect("a wire ends on the ground symbol");
    assert_eq!(
        name_of(connectivity, SheetItem::Wire(into_ground)),
        Some("GND")
    );
    let gnd = connectivity
        .net_of(SheetItem::Wire(into_ground))
        .expect("GND");
    let gnd_pins = net_pins(sheet, connectivity, gnd);
    assert!(gnd_pins.len() > 20, "{}", gnd_pins.len());
    assert!(
        gnd_pins
            .windows(2)
            .all(|pair| pair[0].reference <= pair[1].reference
                || pair[0].reference.len() < pair[1].reference.len()),
        "sorted by reference"
    );
    // R5 at (196.85, 140.97): pin 1 at (196.85, 138.43) ends a wire,
    // pin 2 at (196.85, 143.51) sits on a junction between two wires.
    let r5 = sheet
        .symbols
        .iter()
        .position(|symbol| symbol.reference() == Some("R5"))
        .expect("R5");
    let pin = |pin| SheetItem::Pin {
        symbol: r5,
        unit: 1,
        pin,
    };
    let ends_at = |point: Point| {
        sheet.wires.iter().position(|wire| {
            wire.points
                .iter()
                .any(|p| (p.x - point.x).abs() < 1e-6 && (p.y - point.y).abs() < 1e-6)
        })
    };
    let top = ends_at(Point {
        x: 196.85,
        y: 138.43,
    })
    .expect("a wire at pin 1");
    assert_eq!(
        connectivity.net_of(pin(0)),
        connectivity.net_of(SheetItem::Wire(top))
    );
    let junction = sheet
        .junctions
        .iter()
        .position(|j| (j.at.x - 196.85).abs() < 1e-6 && (j.at.y - 143.51).abs() < 1e-6)
        .expect("the junction at pin 2");
    assert_eq!(
        connectivity.net_of(pin(1)),
        connectivity.net_of(SheetItem::Junction(junction))
    );
    assert_ne!(connectivity.net_of(pin(0)), connectivity.net_of(pin(1)));
    // The SWO label names its net, which reaches a pin.
    let swo = sheet
        .labels
        .iter()
        .position(|label| label.text.text == "SWO")
        .expect("SWO");
    let swo_net = connectivity.net_of(SheetItem::Label(swo)).expect("SWO");
    assert_eq!(
        connectivity.net(swo_net).map(|n| n.name.as_str()),
        Some("SWO")
    );
    assert!(!net_pins(sheet, connectivity, swo_net).is_empty());
    assert!(
        connectivity.nets().len() > 40,
        "{}",
        connectivity.nets().len()
    );
}
