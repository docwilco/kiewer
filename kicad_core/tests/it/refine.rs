//! A region of finer tessellation (#51, ADR 0006): curves inside it are
//! flattened to a finer tolerance, curves outside keep the floor's, and a
//! curve across its edge is one outline of both.

use kicad_core::geometry::Bounds;
use kicad_core::model::{Board, Point};
use kicad_core::parse_board;
use kicad_core::tessellate::{Mesh, Part, Tessellator, Unit};
use test_case::test_case;

/// One micrometre, the floor.
const FLOOR: f64 = 0.001;

fn board(items: &str) -> Board {
    let source = format!(
        r#"(kicad_pcb (version 20241229) (generator "pcbnew")
  (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
  {items}
)"#
    );
    parse_board(&source).expect("the test board parses")
}

/// A through via of 0.4 mm drill at (10, 10) and 0.8 mm across.
fn via_board() -> Board {
    board(r#"(via (at 10 10) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))"#)
}

fn around(x: f64, y: f64, half: f64) -> Bounds {
    Bounds {
        min: Point {
            x: x - half,
            y: y - half,
        },
        max: Point {
            x: x + half,
            y: y + half,
        },
    }
}

fn drill_mesh(board: &Board, tessellator: &Tessellator) -> Mesh {
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    tessellator.holes(board, layer)
}

// The drill is a disc of radius 0.2: a filled disc has one vertex per
// chord. Half a pixel at the top of the step is the tolerance.
#[test_case(0.5 / 1024.0, 45; "the step of 1024 pixels per millimetre")]
#[test_case(0.5 / 4096.0, 90; "the step of 4096")]
fn a_disc_inside_the_region_has_the_chords_of_the_fine_tolerance(fine: f64, chords: usize) {
    let board = via_board();
    let tessellator = Tessellator::new(FLOOR).refined(fine, around(10.0, 10.0, 5.0));
    assert_eq!(drill_mesh(&board, &tessellator).positions.len(), chords);
}

#[test]
fn a_disc_outside_the_region_keeps_the_floors_chords() {
    let board = via_board();
    let tessellator = Tessellator::new(FLOOR).refined(0.5 / 1024.0, around(50.0, 50.0, 5.0));
    assert_eq!(drill_mesh(&board, &tessellator).positions.len(), 32);
}

/// The angle of each vertex of a disc mesh around (10, 10), sorted.
fn angles(mesh: &Mesh) -> Vec<f64> {
    let mut angles: Vec<f64> = mesh
        .positions
        .iter()
        .map(|p| (f64::from(p[1]) - 10.0).atan2(f64::from(p[0]) - 10.0))
        .collect();
    angles.sort_by(f64::total_cmp);
    angles
}

#[test]
fn a_disc_across_the_regions_edge_is_one_outline_fine_inside_and_coarse_outside() {
    let board = via_board();
    // The region holds the right half of the disc: x from 10 up.
    let region = Bounds {
        min: Point { x: 10.0, y: 0.0 },
        max: Point { x: 20.0, y: 20.0 },
    };
    let fine = 0.5 / 1024.0;
    let mesh = drill_mesh(&board, &Tessellator::new(FLOOR).refined(fine, region));
    // Every vertex is on the circle.
    for p in &mesh.positions {
        let radius = (f64::from(p[0]) - 10.0).hypot(f64::from(p[1]) - 10.0);
        assert!(
            (radius - 0.2).abs() < 1e-5,
            "{p:?} is {radius} from the centre"
        );
    }
    let angles = angles(&mesh);
    let gap = |from: f64, to: f64| {
        angles
            .windows(2)
            .filter(|w| w[0] >= from && w[1] <= to)
            .map(|w| w[1] - w[0])
            .fold(0.0_f64, f64::max)
    };
    // Inside (angles within a quarter turn of 0) the gaps are the fine
    // ones, outside the floor's.
    assert!(
        gap(-1.4, 1.4) < std::f64::consts::TAU / 44.0,
        "{}",
        gap(-1.4, 1.4)
    );
    assert!(
        gap(1.8, 3.1) > std::f64::consts::TAU / 40.0,
        "{}",
        gap(1.8, 3.1)
    );
    assert!(
        gap(1.8, 3.1) < std::f64::consts::TAU / 31.0,
        "{}",
        gap(1.8, 3.1)
    );
    // One simple outline: the area lies between the floor's polygon
    // and the disc, with no gap or overlap at the edge.
    let floor = drill_mesh(&board, &Tessellator::new(FLOOR));
    let all_fine = drill_mesh(
        &board,
        &Tessellator::new(FLOOR).refined(fine, around(10.0, 10.0, 5.0)),
    );
    assert!(mesh.area() > floor.area() && mesh.area() < all_fine.area() + 1e-9);
    assert!(mesh.area() < std::f64::consts::PI * 0.04);
}

fn curve_chords(fine: Option<f64>) -> usize {
    let board = board(
        r#"(gr_curve (pts (xy 0 0) (xy 3 0) (xy 6 3) (xy 10 0)) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))"#,
    );
    let tessellator = match fine {
        Some(fine) => Tessellator::new(FLOOR).refined(fine, around(5.0, 1.0, 50.0)),
        None => Tessellator::new(FLOOR),
    };
    // A stroke's vertices are its two sides' per chord and the caps.
    tessellator.outline(&board).positions.len()
}

#[test]
fn a_beziers_chords_grow_as_the_tolerance_shrinks() {
    let floor = curve_chords(None);
    let coarse = curve_chords(Some(0.5 / 1024.0));
    let finer = curve_chords(Some(0.5 / 4096.0));
    assert!(floor < coarse && coarse < finer, "{floor} {coarse} {finer}");
}

/// Two vias, one at (10, 10) and one at (100, 100), a track beside the
/// first, and a zone fill of plain points over the first.
fn units_board() -> Board {
    board(
        r#"(via (at 10 10) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))
  (via (at 100 100) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))
  (segment (start 12 10) (end 14 10) (width 0.25) (layer "F.Cu") (net "GND"))
  (zone (net 0) (net_name "GND") (layer "F.Cu") (uuid "z") (hatch edge 0.5)
    (connect_pads (clearance 0.5)) (min_thickness 0.25) (filled_areas_thickness no)
    (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5))
    (polygon (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30)))
    (filled_polygon (layer "F.Cu") (pts (xy 1 1) (xy 29 1) (xy 29 29) (xy 1 29))))"#,
    )
}

#[test]
fn a_rebuild_makes_again_only_the_curved_units_the_region_reaches() {
    let board = units_board();
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    let floor = Tessellator::new(FLOOR);
    let copper = floor.units(&board, Part::CopperRest(layer), None);
    let copper: Vec<Unit> = copper.into_iter().map(|u| u.expect("built")).collect();
    // Two vias and a track, all with a round part.
    assert_eq!(copper.len(), 3);
    assert!(copper.iter().all(|unit| unit.curved && !unit.fine));
    let refined = floor.refined(0.5 / 1024.0, around(10.0, 10.0, 10.0));
    let again = refined.units(&board, Part::CopperRest(layer), Some(&copper));
    // The order is the board's: segments, then vias.
    assert!(again[0].is_some(), "the track in the region");
    assert!(again[1].is_some(), "the via in the region");
    assert!(again[2].is_none(), "the via out of it is kept");
    let via = again[1].as_ref().expect("rebuilt");
    assert!(via.fine && via.curved);
    assert!(via.mesh.positions.len() > copper[1].mesh.positions.len());
    // The region leaves: the unit built fine is built again, at the floor.
    let kept: Vec<Unit> = copper
        .iter()
        .zip(&again)
        .map(|(old, new)| new.clone().unwrap_or_else(|| old.clone()))
        .collect();
    let away = floor.refined(0.5 / 1024.0, around(100.0, 100.0, 10.0));
    let back = away.units(&board, Part::CopperRest(layer), Some(&kept));
    assert_eq!(
        back[1].as_ref().map(|u| (u.fine, u.mesh.clone())),
        Some((false, copper[1].mesh.clone()))
    );
    assert!(back[0].is_some() && back[2].is_some());
}

#[test]
fn a_zone_fill_of_plain_points_is_never_made_again() {
    let board = units_board();
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    let floor = Tessellator::new(FLOOR);
    let zones: Vec<Unit> = floor
        .units(&board, Part::Zones(layer), None)
        .into_iter()
        .map(|u| u.expect("built"))
        .collect();
    assert_eq!(zones.len(), 1);
    assert!(!zones[0].curved);
    let refined = floor.refined(0.5 / 1024.0, around(10.0, 10.0, 10.0));
    assert!(
        refined
            .units(&board, Part::Zones(layer), Some(&zones))
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn a_unit_is_tagged_with_its_net() {
    let board = units_board();
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    let copper = Tessellator::new(FLOOR).units(&board, Part::CopperRest(layer), None);
    assert!(copper.iter().flatten().all(|unit| unit.net.is_some()));
}

#[test]
fn a_sheet_refines_over_a_region_and_comes_back_to_the_floor() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../viewer-drop/board/midi8knob.kicad_sch"
    ))
    .expect("the hand-off schematic is in the repo");
    let sheet = kicad_core::parse_schematic(&source).expect("it parses");
    let floor = Tessellator::new(FLOOR);
    let mut units = floor.sheet_units(&sheet);
    let at = sheet.junctions.first().expect("a junction").at;
    let fine = floor.refined(0.5 / 1024.0, around(at.x, at.y, 5.0));
    assert!(
        fine.refine_sheet(&sheet, &mut units),
        "the junction is curved"
    );
    assert_ne!(units.layers(), floor.sheet(&sheet));
    assert!(
        floor.refine_sheet(&sheet, &mut units),
        "the fine units go back"
    );
    assert_eq!(units.layers(), floor.sheet(&sheet));
    assert!(
        !floor.refine_sheet(&sheet, &mut units),
        "and nothing is left to do"
    );
}

/// Two fill pieces of a zone on F.Cu: a square of plain points, and a
/// square whose right side bulges out through a semicircle of radius 5
/// around (30, 5), the arc kept in its point list (#30).
fn fills_board() -> Board {
    board(
        r#"(zone (net "GND") (layers "F.Cu") (hatch edge 0.5) (fill yes)
             (polygon (pts (xy 0 0) (xy 40 0) (xy 40 20) (xy 0 20)))
             (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
             (filled_polygon (layer "F.Cu")
               (pts (xy 20 0) (arc (start 30 0) (mid 35 5) (end 30 10)) (xy 20 10))))"#,
    )
}

#[test]
fn a_fill_piece_holding_an_arc_is_made_again_where_a_plain_one_is_not() {
    let board = fills_board();
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    let floor = Tessellator::new(FLOOR);
    let zones: Vec<Unit> = floor
        .units(&board, Part::Zones(layer), None)
        .into_iter()
        .map(|unit| unit.expect("built"))
        .collect();
    assert_eq!(zones.len(), 2);
    assert!(!zones[0].curved, "a piece of plain points");
    assert!(zones[1].curved, "a piece holding an arc");
    // A region over both.
    let refined = floor.refined(0.5 / 1024.0, around(20.0, 5.0, 30.0));
    let again = refined.units(&board, Part::Zones(layer), Some(&zones));
    assert!(again[0].is_none());
    let piece = again[1].as_ref().expect("the arc's piece");
    assert!(piece.mesh.positions.len() > zones[1].mesh.positions.len());
    // The half disc is the same shape either way, finer: its area is
    // nearer the true one.
    let truth = 100.0 + std::f64::consts::PI * 25.0 / 2.0;
    assert!((piece.mesh.area() - truth).abs() < (zones[1].mesh.area() - truth).abs());
}

/// A footprint at (100, 100) with a custom pad whose one primitive is a
/// filled polygon with an arc, written in the pad's own frame around its origin.
fn arc_primitive_board() -> Board {
    board(
        r#"(footprint "X" (layer "F.Cu") (at 100 100)
             (pad "1" smd custom (at 0 0) (size 0.5 0.5) (layers "F.Cu") (net "GND")
               (options (clearance outline) (anchor circle))
               (primitives (gr_poly (pts (xy 0 0) (arc (start 1 0) (mid 1.5 0.5) (end 1 1)) (xy 0 1)) (width 0) (fill yes)))))"#,
    )
}

fn pad_vertices(region: Option<Bounds>) -> usize {
    let board = arc_primitive_board();
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    let tessellator = match region {
        Some(region) => Tessellator::new(FLOOR).refined(0.5 / 1024.0, region),
        None => Tessellator::new(FLOOR),
    };
    tessellator
        .units(&board, Part::CopperRest(layer), None)
        .into_iter()
        .flatten()
        .map(|unit| unit.mesh.positions.len())
        .sum()
}

#[test]
fn a_custom_pads_polygon_primitive_with_an_arc_is_refined_where_the_pad_sits_on_the_board() {
    let floor = pad_vertices(None);
    // The pad is at (100, 100) on the board: a region there reaches it.
    assert!(pad_vertices(Some(around(100.0, 100.0, 5.0))) > floor);
    // A region around the pad-local origin, (0, 0) on the board, does not.
    assert_eq!(pad_vertices(Some(around(0.0, 0.0, 5.0))), floor);
}
