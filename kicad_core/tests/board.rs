//! The hand-off board through `parse_board`: the counts the issue pins,
//! and that every item resolves its layer and net.

use std::sync::OnceLock;

use kicad_core::model::{Board, Layer, LayerKind, NetId, PadKind, Shape, TextRole, ViaKind};
use kicad_core::parse_board;
use test_case::test_case;

const BOARD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);

fn board() -> &'static Board {
    static BOARD: OnceLock<Board> = OnceLock::new();
    BOARD.get_or_init(|| {
        let source =
            std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
        parse_board(&source).expect("the hand-off board parses")
    })
}

#[test]
fn reads_the_header() {
    let board = board();
    assert_eq!(board.version, 20_260_206);
    assert_eq!(board.generator.as_deref(), Some("pcbnew"));
    assert_eq!(board.generator_version.as_deref(), Some("10.0"));
    assert_eq!(board.thickness, Some(1.57448));
}

#[test]
fn pins_the_hand_off_counts() {
    let board = board();
    assert_eq!(board.footprints.len(), 78, "footprints");
    assert_eq!(board.vias.len(), 408, "vias");
    assert_eq!(board.segments.len(), 540, "segments");
    assert_eq!(board.arcs.len(), 0, "track arcs");
    assert_eq!(board.nets.len(), 75, "nets");
    assert_eq!(board.copper_layers().count(), 4, "copper layers");
    assert_eq!(board.layers.len(), 26, "layers");
}

/// The issue says 41 zones. A line-oriented count of `(zone` over the file
/// gives 41 because it also matches the 36 `(zone_connect 2)` pad lines;
/// the board has 5 zones, which carry the 19 fills between them.
#[test]
fn pins_the_zone_and_fill_counts() {
    let board = board();
    assert_eq!(board.all_zones().count(), 5, "zones");
    assert_eq!(board.zones.len(), 5, "board-level zones");
    let fills: usize = board.all_zones().map(|zone| zone.fills.len()).sum();
    assert_eq!(fills, 19, "stored fills");
    assert_eq!(board.all_pads().count(), 296, "pads");
}

#[test]
fn pins_the_text_items_naming_consolas() {
    let board = board();
    let consolas = board
        .all_texts()
        .filter(|text| text.font.face.as_deref() == Some("Consolas"))
        .count();
    assert_eq!(consolas, 89);
    assert_eq!(board.texts.len(), 7, "board-level texts");
}

#[test]
fn copper_layers_are_the_four_signal_layers_in_stack_order() {
    let names: Vec<&str> = board()
        .copper_layers()
        .map(|(_, layer)| layer.name.as_str())
        .collect();
    assert_eq!(names, ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
    assert!(
        board()
            .copper_layers()
            .all(|(_, l)| l.kind == LayerKind::Signal)
    );
    let silk = board().layer_named("F.SilkS").expect("F.SilkS exists");
    assert_eq!(
        board().layer(silk).and_then(|l| l.user_name.as_deref()),
        Some("F.Silkscreen")
    );
}

#[test]
fn every_via_segment_and_zone_carries_a_net_that_resolves() {
    let board = board();
    for via in &board.vias {
        let net = via.net.expect("a via on a net");
        assert!(board.net(net).is_some());
        assert_eq!(via.kind, ViaKind::Through);
        assert!(via.layers.iter().all(|&l| board.layer(l).is_some()));
    }
    for segment in &board.segments {
        let net = segment.net.expect("a segment on a net");
        assert!(board.net(net).is_some());
        assert!(board.layer(segment.layer).is_some_and(Layer::is_copper));
    }
    for zone in board.all_zones() {
        let net = zone.net.expect("a zone on a net");
        assert!(board.net(net).is_some());
        assert!(!zone.layers.is_empty());
        for fill in &zone.fills {
            assert!(
                zone.layers.contains(&fill.layer),
                "a fill on one of the zone's layers"
            );
            assert!(fill.points.len() >= 3);
        }
    }
}

#[test]
fn every_pad_carries_its_net_except_the_unplated_holes() {
    let board = board();
    let mut unconnected = 0;
    for pad in board.all_pads() {
        if let Some(net) = pad.net {
            assert!(board.net(net).is_some());
        } else {
            assert_eq!(pad.kind, PadKind::NpThroughHole);
            unconnected += 1;
        }
        assert!(!pad.layers.is_empty(), "pad {} is on no layer", pad.number);
        assert!(pad.layers.iter().all(|&l| board.layer(l).is_some()));
    }
    assert_eq!(unconnected, 2);
}

#[test]
fn through_hole_pads_are_on_every_copper_layer() {
    let board = board();
    let copper: Vec<_> = board.copper_layers().map(|(id, _)| id).collect();
    let pad = board
        .all_pads()
        .find(|pad| pad.kind == PadKind::ThroughHole)
        .expect("a through-hole pad");
    assert_eq!(pad.layers.len(), 6, "four copper and two mask layers");
    assert_eq!(
        pad.layers.get(..4),
        Some(copper.as_slice()),
        "*.Cu expands in stack order, F.Cu first"
    );
    assert!(pad.drill.is_some());
}

#[test]
fn nets_are_named_and_gnd_is_the_busiest() {
    let board = board();
    let gnd = board
        .nets
        .iter()
        .position(|net| net.name == "GND")
        .expect("GND is a net");
    let on_gnd = board
        .all_pads()
        .filter(|pad| pad.net.is_some_and(|n| n.index() == gnd))
        .count();
    assert!(on_gnd > 50, "{on_gnd} pads on GND");
    assert!(board.nets.iter().all(|net| !net.name.is_empty()));
    assert!(board.nets.iter().any(|net| net.name == "Net-(D15-K)"));
}

#[test]
#[expect(
    clippy::float_cmp,
    reason = "the values are parsed from these very decimal literals, so they are bit-identical"
)]
fn the_first_footprint_is_led6_at_its_place() {
    let fp = &board().footprints[0];
    assert_eq!(fp.name, "LED_SMD:LED_WS2812B_PLCC4_5.0x5.0mm_P3.2mm");
    assert_eq!(fp.reference(), Some("LED6"));
    assert_eq!((fp.at.x, fp.at.y, fp.rotation), (120.0, 53.5, 0.0));
    assert!(fp.attributes.smd);
    assert!(!fp.attributes.dnp);
    assert_eq!(
        board().layer(fp.layer).map(|l| l.name.as_str()),
        Some("F.Cu")
    );
    let reference = fp
        .texts
        .iter()
        .find(|text| text.role == TextRole::Reference)
        .expect("the reference is placed");
    assert_eq!(reference.text, "LED6");
    assert!(reference.knockout);
    assert_eq!(reference.rotation, 90.0);
    assert_eq!(reference.font.face.as_deref(), Some("Consolas"));
    assert_eq!(reference.font.size.width, 1.0);
    assert_eq!(reference.font.thickness, Some(0.15));
    let pad1 = &fp.pads[0];
    assert_eq!(pad1.number, "1");
    assert_eq!(pad1.pin_function.as_deref(), Some("VDD_1"));
    assert_eq!(pad1.round_radius_ratio, Some(0.1));
    assert_eq!(pad1.layers.len(), 3);
}

#[test]
fn the_outline_is_four_lines_and_four_arcs() {
    let outline: Vec<_> = board().outline().collect();
    assert_eq!(outline.len(), 8);
    let lines = outline
        .iter()
        .filter(|g| matches!(g.drawing.shape, Shape::Line { .. }))
        .count();
    let arcs = outline
        .iter()
        .filter(|g| matches!(g.drawing.shape, Shape::Arc { .. }))
        .count();
    assert_eq!((lines, arcs), (4, 4));
    assert!(outline.iter().all(|g| g.drawing.width > 0.0));
}

#[test]
fn custom_pads_carry_their_primitives() {
    let custom: Vec<_> = board()
        .all_pads()
        .filter(|pad| pad.shape == kicad_core::model::PadShape::Custom)
        .collect();
    assert_eq!(custom.len(), 4);
    assert!(custom.iter().all(|pad| !pad.primitives.is_empty()));
    assert!(custom.iter().all(|pad| pad.primitives[0].filled));
}

#[test]
fn the_multi_layer_zone_is_on_all_four_copper_layers() {
    let board = board();
    let zone = board
        .zones
        .iter()
        .find(|zone| zone.layers.len() == 4)
        .expect("the ground zone spans the stack");
    assert_eq!(
        board
            .net(zone.net.expect("on a net"))
            .map(|n| n.name.as_str()),
        Some("GND")
    );
    assert!(zone.filled);
    assert!(!zone.keepout);
    let mut fill_layers: Vec<_> = zone.fills.iter().map(|fill| fill.layer).collect();
    fill_layers.sort();
    fill_layers.dedup();
    assert_eq!(fill_layers.len(), 4, "fills on every one of its layers");
    assert_eq!(zone.fills.len(), 15, "several polygons per layer");
}

#[test]
fn hidden_properties_are_texts_that_are_hidden() {
    let board = board();
    let datasheet = board.footprints[0]
        .texts
        .iter()
        .find(|t| t.role == TextRole::Property("Datasheet".to_owned()))
        .expect("Datasheet is placed");
    assert!(datasheet.hidden);
    assert_eq!(
        board.footprints[0].property("Datasheet"),
        Some(datasheet.text.as_str())
    );
}

/// A KiCad 9 board with a net table and numbered net references: the other
/// way nets are written.
const KICAD9_BOARD: &str = r#"(kicad_pcb
  (version 20241229)
  (generator "pcbnew")
  (generator_version "9.0")
  (general (thickness 1.6))
  (layers
    (0 "F.Cu" signal)
    (2 "B.Cu" signal)
    (7 "B.SilkS" user "B.Silkscreen")
    (25 "Edge.Cuts" user)
    (31 "F.CrtYd" user "F.Courtyard")
    (33 "B.Fab" user)
  )
  (net 0 "")
  (net 1 "GND")
  (net 2 "VCC")
  (segment (start 0 0) (end 1 0) (width 0.25) (layer "F.Cu") (net 2))
  (arc (start 1 0) (mid 1.5 0.5) (end 2 0) (width 0.25) (layer "F.Cu") (net 2))
  (via (at 1 1) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (via blind (at 2 2) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (free yes) (net 0))
  (gr_rect (start 0 0) (end 10 10) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts"))
  (zone (net 1) (net_name "GND") (layers "F&B.Cu") (name "ground") (priority 1)
    (hatch edge 0.5) (min_thickness 0.25)
    (fill yes (thermal_gap 0.5))
    (polygon (pts (xy 0 0) (xy 10 0) (arc (start 10 10) (mid 5 12) (end 0 10))))
    (filled_polygon (layer "B.Cu") (island) (pts (xy 1 1) (xy 9 1) (xy 9 9)))
  )
  (footprint "R:R_0603" (layer "B.Cu") (at 5 5 180)
    (attr through_hole dnp)
    (property "Reference" "R1" (at 0 -1 180) (layer "B.SilkS") (effects (font (size 1 1) (thickness 0.15)) (justify mirror)))
    (property "Value" "10k" (at 0 1 180) (layer "B.Fab") (hide yes) (effects (font (size 1 1) (thickness 0.15))))
    (property "LCSC" "C1234")
    (fp_text user "note" (at 0 0) (layer "B.Fab") (effects (font (size 0.5 0.5) bold italic) (justify right top)))
    (fp_line (start -1 0) (end 1 0) (stroke (width 0.1) (type solid)) (layer "B.SilkS"))
    (pad "1" thru_hole oval (at -1 0 90) (size 1 2) (drill oval 0.5 1 (offset 0.1 0)) (layers "*.Cu" "*.Mask") (net 1 "GND"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "B.Cu" "B.Fab") (net 2 "VCC"))
    (pad "" np_thru_hole circle (at 0 0) (size 1 1) (drill 1) (layers "*.Cu" "*.Mask"))
    (pad "3" thru_hole oval (at 2 0) (size 1 2) (drill oval 0.6) (layers "*.Cu"))
    (pad "4" smd custom (at 3 0) (size 0.5 0.5) (drill (offset 0.2 0)) (layers "B.Cu")
      (primitives (gr_bbox (start -1 -1) (end 1 1)) (gr_circle (center 0 0) (end 0.5 0) (width 0) (fill yes))))
  )
)"#;

#[test]
fn a_kicad9_board_with_a_net_table_resolves_numbered_nets() {
    let board = parse_board(KICAD9_BOARD).expect("the KiCad 9 board parses");
    assert_eq!(board.version, 20_241_229);
    let names: Vec<&str> = board.nets.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(
        names,
        ["GND", "VCC"],
        "net 0 is no net and is not in the table"
    );
    let vcc = board
        .nets
        .iter()
        .position(|n| n.name == "VCC")
        .expect("VCC");
    assert_eq!(board.segments[0].net.map(NetId::index), Some(vcc));
    assert_eq!(board.arcs.len(), 1);
    assert_eq!(board.arcs[0].net.map(NetId::index), Some(vcc));
    assert_eq!(board.vias[0].net.map(NetId::index), Some(0));
    assert_eq!(board.vias[1].net, None, "net 0 is no net");
    assert_eq!(board.vias[1].kind, ViaKind::Blind);
    assert!(board.vias[1].free);
    assert!(!board.vias[0].free);
}

#[test]
#[expect(
    clippy::float_cmp,
    reason = "the values are parsed from these very decimal literals, so they are bit-identical"
)]
fn a_kicad9_board_reads_its_zone_footprint_and_pads() {
    let board = parse_board(KICAD9_BOARD).expect("parses");
    let zone = &board.zones[0];
    assert_eq!(zone.name.as_deref(), Some("ground"));
    assert_eq!(zone.priority, 1);
    assert_eq!(zone.layers.len(), 2, "F&B.Cu is both outer layers");
    assert_eq!(zone.outline.len(), 5, "two points and an arc's three");
    assert_eq!(zone.fills.len(), 1);
    assert!(zone.fills[0].island);

    let fp = &board.footprints[0];
    assert_eq!(fp.reference(), Some("R1"));
    assert_eq!(fp.value(), Some("10k"));
    assert_eq!(fp.property("LCSC"), Some("C1234"));
    assert!(fp.attributes.through_hole && fp.attributes.dnp && !fp.attributes.smd);
    assert_eq!(
        fp.texts.len(),
        3,
        "two placed properties and the user text; LCSC is not placed"
    );
    let note = fp
        .texts
        .iter()
        .find(|t| t.role == TextRole::User)
        .expect("note");
    assert!(note.font.bold && note.font.italic);
    assert_eq!(note.font.face, None);
    assert_eq!(
        note.justify.horizontal,
        kicad_core::model::Horizontal::Right
    );
    assert_eq!(note.justify.vertical, kicad_core::model::Vertical::Top);
    let reference = fp
        .texts
        .iter()
        .find(|t| t.role == TextRole::Reference)
        .expect("R1");
    assert!(reference.justify.mirror);
    assert_eq!(fp.graphics.len(), 1);

    let pad1 = &fp.pads[0];
    assert_eq!(pad1.kind, PadKind::ThroughHole);
    assert_eq!(pad1.shape, kicad_core::model::PadShape::Oval);
    let drill = pad1.drill.expect("a hole");
    assert_eq!((drill.size.width, drill.size.height), (0.5, 1.0));
    assert_eq!(drill.offset.x, 0.1);
    assert_eq!(
        pad1.layers.len(),
        2,
        "two copper layers, no mask layers in this table"
    );
    assert_eq!(pad1.net.map(NetId::index), Some(0));
    assert_eq!(fp.pads[1].net.map(NetId::index), Some(1));
    assert_eq!(fp.pads[2].net, None);
    assert_eq!(fp.pads[2].kind, PadKind::NpThroughHole);

    let slot = fp.pads[3].drill.expect("an oval hole with one size");
    assert_eq!((slot.size.width, slot.size.height), (0.6, 0.6));
    let offset_only = &fp.pads[4];
    assert_eq!(
        offset_only.drill, None,
        "a (drill (offset ...)) with no size is no hole"
    );
    assert_eq!(
        offset_only.primitives.len(),
        1,
        "the gr_bbox hint is not a primitive"
    );
}

#[test_case("F.Cu", true)]
#[test_case("B.Cu", true)]
#[test_case("Edge.Cuts", false)]
#[test_case("F.CrtYd", false)]
fn layers_know_whether_they_are_copper(name: &str, copper: bool) {
    let board = parse_board(KICAD9_BOARD).expect("parses");
    let id = board.layer_named(name).expect("the layer exists");
    assert_eq!(board.layer(id).expect("resolves").is_copper(), copper);
}
