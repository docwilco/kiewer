//! The hand-off schematic through `parse_schematic`: the counts of what
//! the one sheet holds, a known symbol's pins, a known wire's ends, and
//! the placement rules the viewer draws by.

use std::sync::OnceLock;

use kicad_core::model::{Horizontal, Point};
use kicad_core::parse_schematic;
use kicad_core::schematic::model::{
    Fill, FlagShape, LabelKind, Mirror, PaperSize, PinElectrical, PinShape, Sheet, SymbolShape,
};
use test_case::test_case;

const SCHEMATIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
);

fn schematic() -> &'static Sheet {
    static SCHEMATIC: OnceLock<Sheet> = OnceLock::new();
    SCHEMATIC.get_or_init(|| {
        let source =
            std::fs::read_to_string(SCHEMATIC_PATH).expect("the hand-off schematic is in the repo");
        parse_schematic(&source).expect("the hand-off schematic parses")
    })
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn reads_the_header() {
    let sch = schematic();
    assert_eq!(sch.version, 20_260_306);
    assert_eq!(sch.generator.as_deref(), Some("eeschema"));
    assert_eq!(sch.generator_version.as_deref(), Some("10.0"));
    assert_eq!(sch.paper.kind, PaperSize::A4);
    assert!(close(sch.paper.size.width, 297.0) && close(sch.paper.size.height, 210.0));
}

#[test]
fn pins_the_hand_off_counts() {
    let sch = schematic();
    assert_eq!(sch.library.len(), 25, "embedded library symbols");
    assert_eq!(sch.symbols.len(), 114, "placed symbols");
    assert_eq!(sch.wires.len(), 224, "wires");
    assert_eq!(sch.buses.len(), 0, "buses");
    assert_eq!(sch.junctions.len(), 82, "junctions");
    assert_eq!(sch.no_connects.len(), 9, "no-connects");
    assert_eq!(sch.labels.len(), 48, "labels");
    assert!(
        sch.labels
            .iter()
            .all(|label| matches!(label.kind, LabelKind::Global(_))),
        "every label of the hand-off is global"
    );
    assert_eq!(sch.texts.len(), 1, "free texts");
    assert_eq!(sch.sheet_symbols.len(), 0, "sheet symbols");
    assert_eq!(sch.graphics.len(), 0, "sheet-level graphics");
}

#[test]
fn every_symbol_resolves_to_an_embedded_library_symbol() {
    let sch = schematic();
    for symbol in &sch.symbols {
        let library = sch
            .library_symbol(symbol)
            .unwrap_or_else(|| panic!("{} is in lib_symbols", symbol.lib_id));
        assert_eq!(library.name, symbol.lib_id);
    }
    let dnp = sch.symbols.iter().filter(|symbol| symbol.dnp).count();
    assert_eq!(dnp, 15, "do-not-populate symbols");
}

#[test]
fn the_small_resistor_has_a_body_and_two_pins() {
    let sch = schematic();
    let resistor = sch
        .library
        .iter()
        .find(|symbol| symbol.name == "Device:R_Small")
        .expect("R_Small is embedded");
    assert!(!resistor.power);
    assert!(resistor.pin_numbers_hidden && resistor.pin_names_hidden);
    assert!(close(resistor.pin_name_offset, 0.254));
    assert_eq!(resistor.units.len(), 2, "the common body and unit 1's pins");
    let body = resistor
        .units
        .iter()
        .find(|unit| unit.unit == 0)
        .expect("a unit-0 body");
    assert_eq!(body.graphics.len(), 1);
    assert!(matches!(
        body.graphics[0].shape,
        SymbolShape::Rect { start: Point { x, y }, .. } if close(x, -0.762) && close(y, 1.778)
    ));
    assert!(close(body.graphics[0].width, 0.2032));
    assert_eq!(body.graphics[0].fill, Fill::None);
    let pins: Vec<_> = resistor.units.iter().flat_map(|u| &u.pins).collect();
    assert_eq!(pins.len(), 2);
    assert_eq!(pins[0].number, "1");
    assert_eq!(pins[0].name, "");
    assert_eq!(pins[0].electrical, PinElectrical::Passive);
    assert_eq!(pins[0].shape, PinShape::Line);
    assert!(close(pins[0].at.x, 0.0) && close(pins[0].at.y, 2.54));
    assert!(close(pins[0].rotation, 270.0));
    assert!(close(pins[0].length, 0.762));
    assert!(!pins[0].hidden);
    assert_eq!(pins[1].number, "2");
    assert!(close(pins[1].rotation, 90.0));
}

#[test]
fn the_usb_c_symbol_puts_its_pin_names_inside_and_has_a_filled_body() {
    let sch = schematic();
    let usb = sch
        .library
        .iter()
        .find(|symbol| symbol.name == "DocWilco:USB_C_Receptacle_TYPE-C-31-M-12")
        .expect("the receptacle is embedded");
    assert!(!usb.pin_names_hidden);
    assert!(close(usb.pin_name_offset, 1.016));
    assert!(
        usb.units
            .iter()
            .flat_map(|u| &u.graphics)
            .any(|g| g.fill == Fill::Background),
        "the body is filled with the body colour"
    );
    assert!(
        usb.units
            .iter()
            .flat_map(|u| &u.graphics)
            .any(|g| matches!(g.shape, SymbolShape::Arc { .. })),
        "the USB trident has arcs"
    );
}

#[test]
fn power_symbols_are_flagged_and_their_reference_is_hidden() {
    let sch = schematic();
    let gnd = sch
        .library
        .iter()
        .find(|symbol| symbol.name == "power:GND")
        .expect("GND is embedded");
    assert!(gnd.power);
    let placed = sch
        .symbols
        .iter()
        .find(|symbol| symbol.lib_id == "power:GND")
        .expect("a GND is placed");
    let reference = placed
        .fields
        .iter()
        .find(|field| field.name == "Reference")
        .expect("a reference field");
    assert!(reference.text.hidden);
    assert!(reference.text.text.starts_with("#PWR"));
    let value = placed
        .fields
        .iter()
        .find(|field| field.name == "Value")
        .expect("a value field");
    assert!(!value.text.hidden);
    assert_eq!(value.text.text, "GND");
}

#[test]
fn the_first_symbol_is_d23_turned_a_quarter() {
    let sch = schematic();
    let d23 = &sch.symbols[0];
    assert_eq!(d23.lib_id, "Device:D_Small");
    assert!(close(d23.at.x, 240.03) && close(d23.at.y, 119.38));
    assert!(close(d23.rotation, 270.0));
    assert_eq!(d23.mirror, Mirror::None);
    assert_eq!(d23.unit, 1);
    assert_eq!(d23.body_style, 1);
    assert!(!d23.dnp);
    let reference = d23
        .fields
        .iter()
        .find(|field| field.name == "Reference")
        .expect("a reference");
    assert_eq!(reference.text.text, "D23");
    assert!(close(reference.text.rotation, 90.0));
    assert_eq!(reference.text.justify.horizontal, Horizontal::Right);
    assert!(!reference.text.hidden);
    assert!(
        d23.fields
            .iter()
            .find(|field| field.name == "Footprint")
            .is_some_and(|field| field.text.hidden)
    );
}

#[test]
fn a_mirrored_capacitor_is_read_as_mirrored() {
    let sch = schematic();
    let mirrored: Vec<_> = sch
        .symbols
        .iter()
        .filter(|symbol| symbol.mirror != Mirror::None)
        .collect();
    assert_eq!(mirrored.len(), 2);
    assert!(mirrored.iter().all(|symbol| symbol.mirror == Mirror::Y));
    assert_eq!(mirrored[0].lib_id, "Device:C_Small");
}

#[test]
fn the_first_wire_runs_right_along_y_29_21() {
    let sch = schematic();
    let wire = &sch.wires[0];
    assert_eq!(wire.points.len(), 2);
    assert!(close(wire.points[0].x, 123.19) && close(wire.points[0].y, 29.21));
    assert!(close(wire.points[1].x, 127.0) && close(wire.points[1].y, 29.21));
    assert!(close(wire.width, 0.0), "width 0 is the schematic's default");
}

#[test]
fn junctions_and_no_connects_sit_where_the_file_puts_them() {
    let sch = schematic();
    assert!(close(sch.junctions[0].at.x, 29.21) && close(sch.junctions[0].at.y, 161.29));
    assert!(close(sch.junctions[0].diameter, 0.0));
    assert!(close(sch.no_connects[0].x, 52.07) && close(sch.no_connects[0].y, 68.58));
}

#[test]
fn the_swo_label_is_an_input_flag_reading_right_from_its_anchor() {
    let sch = schematic();
    let swo = sch
        .labels
        .iter()
        .find(|label| label.text.text == "SWO")
        .expect("SWO is a label");
    assert_eq!(swo.kind, LabelKind::Global(FlagShape::Input));
    assert!(close(swo.text.at.x, 33.02) && close(swo.text.at.y, 104.14));
    assert!(close(swo.text.rotation, 0.0));
    assert_eq!(swo.text.justify.horizontal, Horizontal::Left);
    assert!(close(swo.text.font.size.height, 1.27));
    assert!(
        swo.fields
            .iter()
            .any(|field| field.name == "Intersheetrefs" && field.text.hidden)
    );
}

#[test]
fn the_free_text_is_two_lines_centred_on_its_anchor() {
    let sch = schematic();
    let text = &sch.texts[0];
    assert!(text.text.starts_with("10u + 100n on VDD/VSS\n"));
    assert!(close(text.at.x, 188.468) && close(text.at.y, 25.146));
    assert_eq!(text.justify, kicad_core::model::Justify::default());
}

/// A small KiCad 9 schematic with what the hand-off lacks: a local
/// label, a hierarchical label, a bus with an entry, a sheet symbol with
/// pins, a graphic line and a derived library symbol.
const KICAD9_SCHEMATIC: &str = r#"(kicad_sch (version 20250114) (generator "eeschema") (generator_version "9.0")
  (uuid "00000000-0000-0000-0000-000000000001")
  (paper "User" 100 80)
  (lib_symbols
    (symbol "Device:R" (pin_numbers (hide yes)) (pin_names (offset 0))
      (property "Reference" "R" (at 0 0 0) (effects (font (size 1.27 1.27))))
      (property "Value" "R" (at 0 0 0) (effects (font (size 1.27 1.27))))
      (symbol "R_0_1" (rectangle (start -1.016 -2.54) (end 1.016 2.54) (stroke (width 0.254) (type default)) (fill (type none))))
      (symbol "R_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27) (name "~" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
        (pin passive line (at 0 -3.81 90) (length 1.27) (name "~" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))))
    (symbol "Device:R_Derived" (extends "R")
      (property "Reference" "R" (at 0 0 0) (effects (font (size 1.27 1.27))))
      (property "Value" "R_Derived" (at 0 0 0) (effects (font (size 1.27 1.27)))))
  )
  (bus (pts (xy 10 10) (xy 50 10)) (stroke (width 0) (type default)) (uuid "b"))
  (bus_entry (at 20 10) (size 2.54 2.54) (stroke (width 0) (type default)) (uuid "e"))
  (wire (pts (xy 22.54 12.54) (xy 22.54 20)) (stroke (width 0) (type default)) (uuid "w"))
  (polyline (pts (xy 1 1) (xy 1 5)) (stroke (width 0) (type default)) (uuid "p"))
  (label "NET1" (at 22.54 20 90) (effects (font (size 1.27 1.27)) (justify left)) (uuid "l"))
  (hierarchical_label "OUT" (shape output) (at 60 20 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "h"))
  (symbol (lib_id "Device:R_Derived") (at 40 40 90) (mirror x) (unit 1) (body_style 1) (dnp yes) (uuid "s")
    (property "Reference" "R1" (at 42 40 90) (effects (font (size 1.27 1.27))))
    (property "Value" "4k7" (at 38 40 90) (effects (font (size 1.27 1.27)))))
  (sheet (at 60 40) (size 20 15) (stroke (width 0.1524) (type solid)) (fill (color 0 0 0 0.0)) (uuid "sh")
    (property "Sheetname" "power" (at 60 39 0) (effects (font (size 1.27 1.27)) (justify left bottom)))
    (property "Sheetfile" "power.kicad_sch" (at 60 55.5 0) (effects (font (size 1.27 1.27)) (justify left top)))
    (pin "VIN" input (at 60 45 180) (effects (font (size 1.27 1.27)) (justify right)) (uuid "sp"))
    (pin "VOUT" output (at 80 45 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "sp2")))
  (sheet_instances (path "/" (page "1")))
)"#;

#[test]
fn a_kicad9_schematic_reads_what_the_hand_off_lacks() {
    let sch = parse_schematic(KICAD9_SCHEMATIC).expect("parses");
    assert_eq!(sch.version, 20_250_114);
    assert!(close(sch.paper.size.width, 100.0) && close(sch.paper.size.height, 80.0));
    assert_eq!(sch.buses.len(), 1);
    assert_eq!(sch.bus_entries.len(), 1);
    assert!(close(sch.bus_entries[0].size.x, 2.54));
    assert_eq!(sch.wires.len(), 1);
    assert_eq!(sch.graphics.len(), 1);
    assert!(matches!(
        sch.graphics[0].shape,
        SymbolShape::Polyline { ref points } if points.len() == 2
    ));
    assert_eq!(sch.labels.len(), 2);
    assert_eq!(sch.labels[0].kind, LabelKind::Local);
    assert!(close(sch.labels[0].text.rotation, 90.0));
    assert_eq!(
        sch.labels[1].kind,
        LabelKind::Hierarchical(FlagShape::Output)
    );
    let symbol = &sch.symbols[0];
    assert_eq!(symbol.mirror, Mirror::X);
    assert!(symbol.dnp);
    let sheet = &sch.sheet_symbols[0];
    assert!(close(sheet.size.width, 20.0) && close(sheet.size.height, 15.0));
    assert_eq!(sheet.pins.len(), 2);
    assert_eq!(sheet.pins[0].text.text, "VIN");
    assert_eq!(sheet.pins[0].shape, FlagShape::Input);
    assert!(close(sheet.pins[0].text.rotation, 180.0));
    assert_eq!(sheet.pins[1].shape, FlagShape::Output);
    assert_eq!(
        sheet.name(),
        Some("power"),
        "the Sheetname field names the sheet"
    );
    assert_eq!(sheet.file(), Some("power.kicad_sch"));
}

#[test]
fn a_derived_library_symbol_inherits_its_parents_body_and_pins() {
    let sch = parse_schematic(KICAD9_SCHEMATIC).expect("parses");
    let derived = sch
        .library_symbol(&sch.symbols[0])
        .expect("R_Derived resolves");
    assert_eq!(derived.name, "Device:R_Derived");
    assert_eq!(derived.extends.as_deref(), Some("R"));
    assert_eq!(derived.units.len(), 2, "the parent's two sub-symbols");
    assert_eq!(derived.units.iter().flat_map(|u| &u.pins).count(), 2);
    assert!(derived.pin_numbers_hidden, "the parent's pin settings too");
}

#[test_case("input", FlagShape::Input)]
#[test_case("output", FlagShape::Output)]
#[test_case("bidirectional", FlagShape::Bidirectional)]
#[test_case("tri_state", FlagShape::TriState)]
#[test_case("passive", FlagShape::Passive)]
fn a_global_label_reads_every_flag_shape(word: &str, shape: FlagShape) {
    let source = format!(
        r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols)
          (global_label "X" (shape {word}) (at 0 0 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "g")))"#
    );
    let sch = parse_schematic(&source).expect("parses");
    assert_eq!(sch.labels[0].kind, LabelKind::Global(shape));
}

#[test]
fn a_symbol_naming_no_embedded_library_symbol_is_an_error() {
    let source = r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols)
      (symbol (lib_id "Device:Ghost") (at 0 0 0) (unit 1) (uuid "s")))"#;
    let error = parse_schematic(source)
        .expect_err("a dangling lib_id is refused")
        .to_string();
    assert!(error.contains("Device:Ghost"), "{error}");
    assert!(error.contains("lib_symbols"), "{error}");
}
