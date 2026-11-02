//! Triangles from a schematic sheet: a symbol body with its pins and
//! texts placed as KiCad places them, the wires and marks, and the label
//! kinds, each on its own colour layer.

use std::sync::OnceLock;

use kicad_core::geometry::Bounds;
use kicad_core::model::Point;
use kicad_core::parse_schematic;
use kicad_core::schematic::model::Sheet;
use kicad_core::tessellate::schematic::SchematicLayer;
use kicad_core::tessellate::{Mesh, Tessellator};
use test_case::test_case;

const SCHEMATIC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
);

fn hand_off() -> &'static Sheet {
    static SCHEMATIC: OnceLock<Sheet> = OnceLock::new();
    SCHEMATIC.get_or_init(|| {
        let source =
            std::fs::read_to_string(SCHEMATIC_PATH).expect("the hand-off schematic is in the repo");
        parse_schematic(&source).expect("the hand-off schematic parses")
    })
}

fn at(x: f64, y: f64) -> Point {
    Point { x, y }
}

/// A sheet holding `items`, with a small resistor symbol in its library:
/// a 1.524 by 3.556 box with pin 1 above (at y 2.54 in the library, so
/// up) and pin 2 below, names hidden and numbers shown outside.
fn sheet(items: &str) -> Sheet {
    let source = format!(
        r#"(kicad_sch (version 20250114) (generator "eeschema") (paper "A4")
  (lib_symbols
    (symbol "Device:R_Small" (pin_numbers (hide no)) (pin_names (offset 0) (hide yes))
      (property "Reference" "R" (at 0 0 0) (effects (font (size 1.27 1.27))))
      (symbol "R_Small_0_1"
        (rectangle (start -0.762 1.778) (end 0.762 -1.778) (stroke (width 0.2032) (type default)) (fill (type {fill}))))
      (symbol "R_Small_1_1"
        (pin passive line (at 0 2.54 270) (length 0.762) (name "" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
        (pin passive line (at 0 -2.54 90) (length 0.762) (name "" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))))
    (symbol "Device:Gate" (pin_names (offset 0.508))
      (property "Reference" "U" (at 0 0 0) (effects (font (size 1.27 1.27))))
      (symbol "Gate_0_1"
        (rectangle (start -2.54 2.54) (end 2.54 -2.54) (stroke (width 0.254) (type default)) (fill (type background)))
        (text "RGB" (at 0 -1.5 0) (effects (font (size 0.762 0.762)))))
      (symbol "Gate_1_1"
        (pin input inverted (at -5.08 0 0) (length 2.54) (name "IN" (effects (font (size 1.27 1.27)))) (number "1" (effects (font (size 1.27 1.27)))))
        (pin output line (at 5.08 0 180) (length 2.54) (name "OUT" (effects (font (size 1.27 1.27)))) (number "2" (effects (font (size 1.27 1.27)))))))
  )
  {items}
)"#,
        fill = if items.contains("FILLED") {
            "background"
        } else {
            "none"
        },
    );
    parse_schematic(&source).expect("the test sheet parses")
}

fn layers(sch: &Sheet) -> Vec<(SchematicLayer, Mesh)> {
    Tessellator::new(0.01).sheet(sch)
}

fn layer(sch: &Sheet, wanted: SchematicLayer) -> Mesh {
    layers(sch)
        .into_iter()
        .find(|(layer, _)| *layer == wanted)
        .map(|(_, mesh)| mesh)
        .unwrap_or_default()
}

fn bounds(mesh: &Mesh) -> Bounds {
    mesh.bounds().expect("the mesh has vertices")
}

#[test]
fn a_symbol_body_is_its_rectangle_stroked_in_place_and_its_pins_drawn_outward() {
    let sch = sheet(
        r#"(symbol (lib_id "Device:R_Small") (at 100 100 0) (unit 1) (uuid "r")
             (property "Reference" "R1" (at 101 99 0) (effects (font (size 1.27 1.27)) (justify left))))"#,
    );
    let device = layer(&sch, SchematicLayer::Device);
    // The right edge of the box, stroked 0.2032 wide, and nothing inside.
    assert!(device.covers(at(100.762, 100.0)), "{:?}", bounds(&device));
    assert!(
        device.covers(at(100.0, 98.222)),
        "the top edge is above (smaller y)"
    );
    assert!(!device.covers(at(100.0, 100.0)), "no fill");
    assert_eq!(layer(&sch, SchematicLayer::Background).indices.len(), 0);
    // Pin 1 runs from (100, 97.46) up at the body's top edge; pin 2 below.
    let pins = layer(&sch, SchematicLayer::Pin);
    assert!(pins.covers(at(100.0, 97.8)), "{:?}", bounds(&pins));
    assert!(pins.covers(at(100.0, 102.2)));
    assert!(!pins.covers(at(100.0, 100.0)), "pins stop at the body");
    // The numbers sit beside the pins, outside the body, and the names
    // are hidden.
    let numbers = layer(&sch, SchematicLayer::PinNumber);
    let number_bounds = bounds(&numbers);
    assert!(number_bounds.min.y < 98.222 && number_bounds.max.y > 101.778);
    assert!(layer(&sch, SchematicLayer::PinName).is_empty());
    // The reference reads to the right of its anchor.
    let reference = layer(&sch, SchematicLayer::Reference);
    let reference_bounds = bounds(&reference);
    assert!(reference_bounds.min.x > 101.0, "{reference_bounds:?}");
    assert!(reference_bounds.max.x < 104.0, "{reference_bounds:?}");
    assert!(
        (reference_bounds.center().y - 99.0).abs() < 0.3,
        "{reference_bounds:?}"
    );
}

#[test]
fn a_filled_body_is_painted_on_the_background_layer_under_its_outline() {
    let sch = sheet(
        r#"(symbol (lib_id "Device:R_Small") (at 100 100 0) (unit 1) (uuid "r") (property "FILLED" "" (at 0 0 0) (hide yes)))"#,
    );
    let order: Vec<SchematicLayer> = layers(&sch).into_iter().map(|(layer, _)| layer).collect();
    let background = order
        .iter()
        .position(|l| *l == SchematicLayer::Background)
        .expect("a background layer");
    let device = order
        .iter()
        .position(|l| *l == SchematicLayer::Device)
        .expect("a device layer");
    assert!(background < device, "{order:?}");
    assert!(layer(&sch, SchematicLayer::Background).covers(at(100.0, 100.0)));
}

#[test_case(90.0, at(97.46, 100.0), at(97.8, 100.0); "a quarter turn puts pin 1 on the left")]
#[test_case(180.0, at(100.0, 102.54), at(100.0, 102.2); "a half turn puts pin 1 below")]
#[test_case(270.0, at(102.54, 100.0), at(102.2, 100.0); "three quarters put pin 1 on the right")]
fn a_turned_symbol_turns_its_pins_counter_clockwise(rotation: f64, tip: Point, along: Point) {
    let sch = sheet(&format!(
        r#"(symbol (lib_id "Device:R_Small") (at 100 100 {rotation}) (unit 1) (uuid "r"))"#
    ));
    let pins = layer(&sch, SchematicLayer::Pin);
    assert!(pins.covers(tip), "{:?}", bounds(&pins));
    assert!(pins.covers(along), "{:?}", bounds(&pins));
}

#[test]
fn a_mirrored_symbol_flips_its_pins_left_to_right() {
    let sch =
        sheet(r#"(symbol (lib_id "Device:Gate") (at 100 100 0) (mirror y) (unit 1) (uuid "g"))"#);
    let pins = layer(&sch, SchematicLayer::Pin);
    // The input pin, at library (-5.08, 0) pointing right, is now on the
    // right pointing left; its inversion bubble sits at the body's edge.
    assert!(pins.covers(at(104.5, 100.0)), "{:?}", bounds(&pins));
    assert!(
        pins.covers(at(103.175, 100.635)),
        "the bubble's lower point"
    );
    assert!(!pins.covers(at(102.8, 100.0)), "the bubble is hollow");
}

#[test]
fn pin_names_sit_inside_the_body_at_the_offset_and_numbers_above_the_pins() {
    let sch = sheet(r#"(symbol (lib_id "Device:Gate") (at 100 100 0) (unit 1) (uuid "g"))"#);
    let names = bounds(&layer(&sch, SchematicLayer::PinName));
    // IN starts 0.508 inside the left edge (x 97.46) and reads right; OUT
    // ends 0.508 inside the right edge.
    assert!(names.min.x > 97.9 && names.min.x < 98.6, "{names:?}");
    assert!(names.max.x < 102.1 && names.max.x > 101.4, "{names:?}");
    assert!(
        (names.center().y - 100.0).abs() < 0.2,
        "centred on the pins"
    );
    let numbers = bounds(&layer(&sch, SchematicLayer::PinNumber));
    assert!(numbers.max.y < 100.0, "above the pins: {numbers:?}");
    assert!(
        numbers.min.x < 97.46 && numbers.max.x > 102.54,
        "over the pin lines"
    );
    // The body text "RGB" is drawn in the body, in the device colour,
    // centred on its (transformed) anchor.
    let device = bounds(&layer(&sch, SchematicLayer::Device));
    assert!(device.min.x < 97.5 && device.max.x > 102.5);
    let text = layer(&sch, SchematicLayer::Device);
    assert!(
        text.covers(at(100.0, 101.5))
            || text.covers(at(100.1, 101.5))
            || text.covers(at(99.9, 101.5)),
        "the G of RGB is near (100, 101.5)"
    );
}

#[test]
fn a_field_of_a_turned_symbol_reads_horizontally_when_it_was_vertical() {
    // D23 in the hand-off: the symbol is turned 270 and its reference
    // field is written at 90; on the sheet it reads horizontally, as
    // KiCad swaps the orientation with the symbol's.
    let sch = sheet(
        r#"(symbol (lib_id "Device:R_Small") (at 100 100 270) (unit 1) (uuid "r")
             (property "Reference" "MMM" (at 105.334 100 90) (effects (font (size 1.27 1.27)) (justify right))))"#,
    );
    let reference = bounds(&layer(&sch, SchematicLayer::Reference));
    assert!(
        reference.width() > 2.0 * reference.height(),
        "{reference:?}"
    );
    assert!((reference.center().y - 100.0).abs() < 0.2, "{reference:?}");
}

#[test]
fn a_right_justified_vertical_field_on_a_turned_symbol_ends_at_its_anchor() {
    // D6 in the hand-off: the diode is turned 270 and its reference,
    // written right-justified at 90, sits left of the body with its
    // anchor 1.524 left of the symbol; KiCad draws the text ending at
    // the anchor, so it stays left of the diode.
    let sch = sheet(
        r#"(symbol (lib_id "Device:R_Small") (at 100 100 270) (unit 1) (uuid "r")
             (property "Reference" "MMM" (at 98.476 100 90) (effects (font (size 1.27 1.27)) (justify right))))"#,
    );
    let reference = bounds(&layer(&sch, SchematicLayer::Reference));
    assert!((reference.max.x - 98.476).abs() < 0.3, "{reference:?}");
    assert!(reference.min.x < 96.0, "{reference:?}");
}

#[test]
fn a_do_not_populate_symbol_gets_a_cross_over_its_body() {
    let sch =
        sheet(r#"(symbol (lib_id "Device:R_Small") (at 100 100 0) (unit 1) (dnp yes) (uuid "r"))"#);
    let dnp = layer(&sch, SchematicLayer::Dnp);
    assert!(
        dnp.covers(at(100.0, 100.0)),
        "the diagonals cross the centre"
    );
    let b = bounds(&dnp);
    assert!(b.min.x < 99.238 && b.max.x > 100.762, "wider than the body");
    assert!(b.min.y < 98.222 && b.max.y > 101.778);
    assert!(
        layer(
            &sheet(r#"(symbol (lib_id "Device:R_Small") (at 100 100 0) (unit 1) (uuid "r"))"#),
            SchematicLayer::Dnp
        )
        .is_empty()
    );
}

#[test]
fn wires_buses_junctions_and_no_connects_draw_at_their_default_sizes() {
    let sch = sheet(
        r#"(wire (pts (xy 10 10) (xy 20 10)) (stroke (width 0) (type default)) (uuid "w"))
           (bus (pts (xy 10 20) (xy 20 20)) (stroke (width 0) (type default)) (uuid "b"))
           (bus_entry (at 15 20) (size 2.54 -2.54) (stroke (width 0) (type default)) (uuid "e"))
           (junction (at 20 10) (diameter 0) (color 0 0 0 0) (uuid "j"))
           (no_connect (at 30 10) (uuid "n"))"#,
    );
    let wire = layer(&sch, SchematicLayer::Wire);
    assert!(wire.covers(at(15.0, 10.0)));
    assert!(
        wire.covers(at(16.27, 18.73)),
        "the bus entry is drawn as a wire"
    );
    let wire_bounds = bounds(&wire);
    assert!(
        (wire_bounds.max.y - 20.0 - 0.1524 / 2.0).abs() < 1e-3,
        "6 mil wide: {wire_bounds:?}"
    );
    let bus = bounds(&layer(&sch, SchematicLayer::Bus));
    assert!((bus.height() - 0.3048).abs() < 1e-3, "12 mil wide: {bus:?}");
    let junction = layer(&sch, SchematicLayer::Junction);
    assert!(junction.covers(at(20.0, 10.0)));
    assert!(
        (bounds(&junction).width() - 0.9144).abs() < 1e-3,
        "36 mil across"
    );
    let no_connect = layer(&sch, SchematicLayer::NoConnect);
    assert!(no_connect.covers(at(30.0, 10.0)));
    assert!(no_connect.covers(at(30.4, 10.4)) && no_connect.covers(at(29.6, 10.4)));
    assert!(!no_connect.covers(at(30.4, 10.0)));
    assert!(
        (bounds(&no_connect).width() - 1.2192).abs() < 0.2,
        "48 mil across"
    );
}

#[test]
fn a_global_input_label_is_a_flag_pointed_at_its_anchor_with_the_text_past_the_tip() {
    let sch = sheet(
        r#"(global_label "IN" (shape input) (at 50 50 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "g"))"#,
    );
    let label = layer(&sch, SchematicLayer::GlobalLabel);
    assert!(label.covers(at(50.0, 50.0)), "the tip is at the anchor");
    let b = bounds(&label);
    assert!(
        b.min.x > 49.5 && b.min.x < 50.0,
        "nothing left of the anchor but the stroke: {b:?}"
    );
    // halfSize = 0.635 + 0.476 = 1.111, plus the pen: the box is about
    // 1.2 above and below the wire.
    assert!((b.min.y - (50.0 - 1.2)).abs() < 0.15, "{b:?}");
    assert!((b.max.y - (50.0 + 1.2)).abs() < 0.15, "{b:?}");
    // The box's flat sides start one halfSize in from the tip.
    assert!(
        label.covers(at(51.111, 48.85)) || label.covers(at(51.2, 48.85)),
        "the upper edge"
    );
    assert!(
        !label.covers(at(50.3, 48.9)),
        "the slant has not reached the top yet"
    );
    // The text starts past the tip: margin plus three quarters of the height.
    // Just off the wire's line: a zero-area triangle of the round join at
    // the tip lies along it, and `covers` counts a point on such a line.
    assert!(
        !label.covers(at(50.8, 50.05)),
        "nothing between the tip and the text"
    );
    assert!(b.max.x > 50.0 + 1.43 + 1.0, "the text is drawn: {b:?}");
}

#[test]
fn a_global_output_label_turned_half_way_has_its_text_on_the_left_and_the_tip_away() {
    let sch = sheet(
        r#"(global_label "OUT" (shape output) (at 50 50 180) (effects (font (size 1.27 1.27)) (justify right)) (uuid "g"))"#,
    );
    let label = layer(&sch, SchematicLayer::GlobalLabel);
    let b = bounds(&label);
    assert!(b.max.x < 50.2, "everything is left of the anchor: {b:?}");
    assert!(label.covers(at(50.0, 49.0)), "the anchor side is flat");
    assert!(
        label.covers(at(b.min.x + 0.05, 50.0)),
        "the far end is a tip on the wire's line"
    );
    assert!(!label.covers(at(b.min.x + 0.3, 49.0)), "the tip is pointed");
}

#[test]
fn a_local_label_stands_on_its_wire_and_a_vertical_one_reads_upward() {
    let sch = sheet(
        r#"(label "A" (at 50 50 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "l"))
           (label "B" (at 70 50 90) (effects (font (size 1.27 1.27)) (justify left)) (uuid "m"))"#,
    );
    let label = layer(&sch, SchematicLayer::LocalLabel);
    let a = Bounds::around(
        label
            .positions
            .iter()
            .map(|p| at(f64::from(p[0]), f64::from(p[1])))
            .filter(|p| p.x < 60.0),
    )
    .expect("A is drawn");
    assert!(a.max.y < 50.0, "above the wire: {a:?}");
    assert!(a.min.x > 50.0, "right of the anchor: {a:?}");
    let b = Bounds::around(
        label
            .positions
            .iter()
            .map(|p| at(f64::from(p[0]), f64::from(p[1])))
            .filter(|p| p.x > 60.0),
    )
    .expect("B is drawn");
    assert!(b.max.y < 50.0, "above the anchor: {b:?}");
    assert!(b.max.x < 70.0, "left of the wire: {b:?}");
}

#[test]
fn a_hierarchical_label_and_a_sheet_pin_draw_their_flags_and_text() {
    let sch = sheet(
        r#"(hierarchical_label "H" (shape input) (at 50 50 0) (effects (font (size 1.27 1.27)) (justify left)) (uuid "h"))
           (sheet (at 60 60) (size 20 10) (stroke (width 0.1524) (type solid)) (uuid "s")
             (property "Sheetname" "sub" (at 60 59.5 0) (effects (font (size 1.27 1.27)) (justify left bottom)))
             (property "Sheetfile" "sub.kicad_sch" (at 60 70.5 0) (effects (font (size 1.27 1.27)) (justify left top)))
             (pin "P" input (at 60 65 180) (effects (font (size 1.27 1.27)) (justify right)) (uuid "p")))"#,
    );
    let hier = layer(&sch, SchematicLayer::HierLabel);
    assert!(
        hier.covers(at(50.0, 50.0)),
        "the input flag's tip is at the anchor"
    );
    assert!(
        hier.covers(at(51.27, 49.365)),
        "its box is one text height long"
    );
    let hb = bounds(&hier);
    assert!(
        hb.max.x > 51.27 + 0.19 + 0.5,
        "the text follows the flag: {hb:?}"
    );
    let sheet_mesh = layer(&sch, SchematicLayer::SheetSymbol);
    assert!(sheet_mesh.covers(at(70.0, 60.0)) && sheet_mesh.covers(at(80.0, 65.0)));
    assert!(
        !sheet_mesh.covers(at(70.0, 65.0)),
        "the sheet is not filled"
    );
    assert!(bounds(&layer(&sch, SchematicLayer::SheetName)).max.y < 60.0);
    assert!(bounds(&layer(&sch, SchematicLayer::SheetFile)).min.y > 70.0);
    let pin = layer(&sch, SchematicLayer::SheetLabel);
    assert!(
        pin.covers(at(60.0, 65.0)),
        "the pin's flag touches the edge"
    );
    assert!(bounds(&pin).max.x > 61.27, "the text is inside the sheet");
}

#[test_case(SchematicLayer::Background, true; "body fills")]
#[test_case(SchematicLayer::Device, true; "bodies")]
#[test_case(SchematicLayer::Notes, true; "the note")]
#[test_case(SchematicLayer::Wire, true; "wires")]
#[test_case(SchematicLayer::GlobalLabel, true; "global labels")]
#[test_case(SchematicLayer::NoConnect, true; "no-connects")]
#[test_case(SchematicLayer::Junction, true; "junctions")]
#[test_case(SchematicLayer::Pin, true; "pins")]
#[test_case(SchematicLayer::PinName, true; "pin names")]
#[test_case(SchematicLayer::PinNumber, true; "pin numbers")]
#[test_case(SchematicLayer::Value, true; "values")]
#[test_case(SchematicLayer::Reference, true; "references")]
#[test_case(SchematicLayer::Dnp, true; "DNP crosses")]
#[test_case(SchematicLayer::Bus, false; "no buses")]
#[test_case(SchematicLayer::LocalLabel, false; "no local labels")]
#[test_case(SchematicLayer::HierLabel, false; "no hierarchical labels")]
#[test_case(SchematicLayer::SheetSymbol, false; "no sheet symbols")]
fn the_hand_off_sheet_draws_the_layers_it_has(layer: SchematicLayer, drawn: bool) {
    let used: Vec<SchematicLayer> = layers(hand_off())
        .into_iter()
        .map(|(layer, _)| layer)
        .collect();
    assert_eq!(used.contains(&layer), drawn, "{used:?}");
}

#[test]
fn the_hand_off_sheet_comes_in_draw_order_and_stays_on_its_paper() {
    let layers = layers(hand_off());
    let used: Vec<SchematicLayer> = layers.iter().map(|(layer, _)| *layer).collect();
    assert!(
        used.windows(2).all(|pair| pair[0] < pair[1]),
        "in draw order: {used:?}"
    );
    let total: usize = layers.iter().map(|(_, mesh)| mesh.indices.len()).sum();
    assert!(total > 100_000, "{total} indices");
    let all = Tessellator::new(0.01)
        .sheet_bounds(hand_off())
        .expect("the sheet has content");
    assert!(all.min.x > 0.0 && all.min.y > 0.0, "{all:?}");
    assert!(all.max.x < 297.0 && all.max.y < 210.0, "{all:?}");
}
