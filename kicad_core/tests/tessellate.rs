//! Tessellation through the public API: the mesh of a small board has the
//! area and the bounds its geometry says it should, independently of how
//! the triangles fall. Areas are the formulas (a stadium is a rectangle and
//! a circle; an annulus is two circles); the tolerance bounds how far a
//! flattened curve may sit from the true one.

use std::sync::OnceLock;

use kicad_core::geometry::Bounds;
use kicad_core::model::{Board, Point};
use kicad_core::parse_board;
use kicad_core::tessellate::{Mesh, Tessellator};
use std::f64::consts::PI;

const BOARD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);

/// One micrometre: the tessellator the viewer uses.
const TOLERANCE: f64 = 0.001;

fn hand_off_board() -> &'static Board {
    static BOARD: OnceLock<Board> = OnceLock::new();
    BOARD.get_or_init(|| {
        let source =
            std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo");
        parse_board(&source).expect("the hand-off board parses")
    })
}

/// A board with four copper layers and an outline layer, holding `items`.
fn board(items: &str) -> Board {
    let source = format!(
        r#"(kicad_pcb (version 20241229) (generator "pcbnew")
  (layers (0 "F.Cu" signal) (4 "In1.Cu" signal) (6 "In2.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
  {items}
)"#
    );
    parse_board(&source).expect("the test board parses")
}

fn f_cu(board: &Board) -> Mesh {
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    Tessellator::new(TOLERANCE).copper(board, layer)
}

fn holes(board: &Board) -> Mesh {
    let layer = board.layer_named("F.Cu").expect("F.Cu");
    Tessellator::new(TOLERANCE).holes(board, layer)
}

fn assert_close(actual: f64, expected: f64, relative: f64, what: &str) {
    let error = (actual - expected).abs() / expected.abs();
    assert!(
        error <= relative,
        "{what}: {actual} is {:.3}% off {expected}",
        error * 100.0
    );
}

fn assert_bounds(mesh: &Mesh, min: (f64, f64), max: (f64, f64), tolerance: f64) {
    let bounds = mesh.bounds().expect("a mesh with vertices");
    let close = |a: f64, b: f64| (a - b).abs() <= tolerance;
    assert!(
        close(bounds.min.x, min.0)
            && close(bounds.min.y, min.1)
            && close(bounds.max.x, max.0)
            && close(bounds.max.y, max.1),
        "bounds {bounds:?}, expected {min:?} to {max:?}"
    );
}

#[test]
fn a_track_is_a_stadium_with_round_caps() {
    let board =
        board(r#"(segment (start 10 20) (end 14 20) (width 0.5) (layer "F.Cu") (net "GND"))"#);
    let mesh = f_cu(&board);
    // Length 4, width 0.5: the rectangle plus the two half-discs.
    assert_close(
        mesh.area(),
        4.0 * 0.5 + PI * 0.25 * 0.25,
        0.01,
        "track area",
    );
    assert_bounds(&mesh, (9.75, 19.75), (14.25, 20.25), TOLERANCE * 2.0);
}

#[test]
fn a_track_on_another_layer_is_not_in_this_one() {
    let board =
        board(r#"(segment (start 10 20) (end 14 20) (width 0.5) (layer "B.Cu") (net "GND"))"#);
    let mesh = f_cu(&board);
    assert!(mesh.is_empty(), "{} vertices", mesh.positions.len());
}

#[test]
fn a_track_arc_sweeps_a_quarter_circle() {
    // A quarter circle of radius 10 around (0, 0), from (10, 0) through
    // 45 degrees to (0, 10): the arc length is 5 pi, the caps add a disc.
    let board = board(
        r#"(arc (start 10 0) (mid 7.0710678 7.0710678) (end 0 10) (width 0.4) (layer "F.Cu") (net "GND"))"#,
    );
    let mesh = f_cu(&board);
    assert_close(
        mesh.area(),
        5.0 * PI * 0.4 + PI * 0.2 * 0.2,
        0.01,
        "arc area",
    );
    assert_bounds(&mesh, (-0.2, -0.2), (10.2, 10.2), 0.01);
}

#[test]
fn a_via_is_an_annulus_around_its_hole() {
    let board =
        board(r#"(via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))"#);
    let mesh = f_cu(&board);
    assert_close(
        mesh.area(),
        PI * (0.4 * 0.4 - 0.2 * 0.2),
        0.01,
        "annulus area",
    );
    assert_bounds(&mesh, (49.6, 49.6), (50.4, 50.4), TOLERANCE * 2.0);
    // The hole is empty: no triangle covers the centre.
    assert!(!mesh.covers(Point { x: 50.0, y: 50.0 }), "the hole is open");
    assert!(
        mesh.covers(Point { x: 50.3, y: 50.0 }),
        "the ring is copper"
    );
}

#[test]
fn a_track_ending_in_a_via_covers_the_hole_and_the_hole_mesh_reopens_it() {
    let board = board(
        r#"(segment (start 40 50) (end 50 50) (width 0.25) (layer "F.Cu") (net "GND"))
           (via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))"#,
    );
    let centre = Point { x: 50.0, y: 50.0 };
    // The copper alone has the track's round cap over the hole: that is
    // why the holes are a mesh of their own, drawn after it.
    assert!(
        f_cu(&board).covers(centre),
        "the track cap reaches the centre"
    );
    let holes = holes(&board);
    assert_close(holes.area(), PI * 0.2 * 0.2, 0.01, "one drill's area");
    assert!(holes.covers(centre), "the hole is drawn over the track");
    assert!(
        !holes.covers(Point { x: 50.3, y: 50.0 }),
        "the ring is not a hole"
    );
}

#[test]
fn a_blind_via_drills_no_hole_through_a_layer_it_does_not_reach() {
    let board = board(
        r#"(via blind (at 50 50) (size 0.8) (drill 0.4) (layers "In1.Cu" "B.Cu") (net "GND"))"#,
    );
    assert!(holes(&board).is_empty());
}

#[test]
fn a_slot_hole_is_drilled_where_its_pad_turns_it() {
    // A 1 wide, 2 tall slot in a pad turned 90 degrees, offset 0.5 mm along
    // the pad's x: on the board the slot lies along x (as the oval pad
    // above turns), and the offset turns the same way, to -y.
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 10 10)
             (pad "1" thru_hole oval (at 0 0 90) (size 2 3) (drill oval 1 2 (offset 0.5 0)) (layers "*.Cu" "*.Mask")))"#,
    );
    let holes = holes(&board);
    assert_close(
        holes.area(),
        PI * 0.5 * 0.5 + 1.0 * 1.0,
        0.01,
        "a stadium: a 1 mm disc plus a 1 x 1 mm middle",
    );
    assert_bounds(&holes, (9.0, 9.0), (11.0, 10.0), TOLERANCE * 2.0);
}

#[test]
fn an_unplated_hole_is_still_a_hole() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "" np_thru_hole circle (at 0 0) (size 0.75 0.75) (drill 0.75) (layers "*.Cu" "*.Mask")))"#,
    );
    assert_close(
        holes(&board).area(),
        PI * 0.375 * 0.375,
        0.01,
        "the drill's area",
    );
}

#[test]
fn a_through_via_is_on_every_copper_layer_and_a_blind_one_is_not() {
    let board = board(
        r#"(via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net "GND"))
           (via blind (at 60 50) (size 0.8) (drill 0.4) (layers "In1.Cu" "B.Cu") (net "GND"))"#,
    );
    let tessellator = Tessellator::new(TOLERANCE);
    let in2 = board.layer_named("In2.Cu").expect("In2.Cu");
    let mesh = tessellator.copper(&board, in2);
    assert!(mesh.covers(Point { x: 50.3, y: 50.0 }), "the through via");
    assert!(
        mesh.covers(Point { x: 60.3, y: 50.0 }),
        "the blind via spans In1 to B"
    );
    let f = board.layer_named("F.Cu").expect("F.Cu");
    let mesh = tessellator.copper(&board, f);
    assert!(mesh.covers(Point { x: 50.3, y: 50.0 }), "the through via");
    assert!(
        !mesh.covers(Point { x: 60.3, y: 50.0 }),
        "the blind via stops at In1"
    );
}

#[test]
fn a_circular_pad_with_a_plated_hole_is_an_annulus() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 100 100)
             (pad "1" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers "*.Cu" "*.Mask") (net "GND")))"#,
    );
    let mesh = f_cu(&board);
    assert_close(mesh.area(), PI * (0.8 * 0.8 - 0.4 * 0.4), 0.01, "pad area");
    assert_bounds(&mesh, (99.2, 99.2), (100.8, 100.8), TOLERANCE * 2.0);
    assert!(
        !mesh.covers(Point { x: 100.0, y: 100.0 }),
        "the hole is open"
    );
}

#[test]
fn an_oval_pad_is_a_stadium_turned_with_its_angle() {
    // 3 wide, 6 tall at 90 degrees: 6 wide and 3 tall on the board. The
    // slot hole 2 by 2.8 turns with it.
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 100 100)
             (pad "MP" thru_hole oval (at 0 0 90) (size 3 6) (drill oval 2 2.8) (layers "*.Cu" "*.Mask") (net "GND")))"#,
    );
    let mesh = f_cu(&board);
    let stadium = |w: f64, h: f64| {
        let r = w.min(h) / 2.0;
        (w.max(h) - 2.0 * r) * 2.0 * r + PI * r * r
    };
    assert_close(
        mesh.area(),
        stadium(3.0, 6.0) - stadium(2.0, 2.8),
        0.01,
        "oval pad area",
    );
    assert_bounds(&mesh, (97.0, 98.5), (103.0, 101.5), TOLERANCE * 2.0);
    assert!(
        !mesh.covers(Point { x: 101.2, y: 100.0 }),
        "inside the turned slot"
    );
    assert!(
        mesh.covers(Point { x: 101.2, y: 101.2 }),
        "copper beside the slot"
    );
}

#[test]
fn a_pad_sits_in_its_footprint_frame() {
    // The footprint at (100, 100) turned 90 degrees: a pad at (5, 0) in
    // its frame lands at (100, 95), since positive angles turn
    // counter-clockwise on the screen with y down.
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 100 100 90)
             (pad "1" smd circle (at 5 0 90) (size 1 1) (layers "F.Cu") (net "GND")))"#,
    );
    let mesh = f_cu(&board);
    assert_bounds(&mesh, (99.5, 94.5), (100.5, 95.5), TOLERANCE * 2.0);
}

#[test]
fn a_rounded_rectangle_pad_loses_its_corners() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "1" smd roundrect (at 0 0) (size 2 1) (layers "F.Cu") (roundrect_rratio 0.25) (net "GND")))"#,
    );
    let mesh = f_cu(&board);
    // Radius 0.25 of the shorter side 1: each corner loses (1 - pi/4) r^2.
    let r = 0.25;
    assert_close(
        mesh.area(),
        2.0 - 4.0 * (1.0 - PI / 4.0) * r * r,
        0.01,
        "rounded rectangle area",
    );
    assert_bounds(&mesh, (-1.0, -0.5), (1.0, 0.5), TOLERANCE * 2.0);
    assert!(
        !mesh.covers(Point { x: 0.99, y: 0.49 }),
        "the corner is gone"
    );
    assert!(
        mesh.covers(Point { x: 0.9, y: 0.4 }),
        "inside the corner's arc"
    );
}

#[test]
fn a_rectangular_pad_turned_45_degrees_is_a_diamond() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "1" smd rect (at 0 0 45) (size 2 2) (layers "F.Cu") (net "GND")))"#,
    );
    let mesh = f_cu(&board);
    assert_close(mesh.area(), 4.0, 1e-6, "rect area");
    let half_diagonal = 2.0_f64.sqrt();
    assert_bounds(
        &mesh,
        (-half_diagonal, -half_diagonal),
        (half_diagonal, half_diagonal),
        1e-6,
    );
    assert!(
        !mesh.covers(Point { x: 0.9, y: 0.9 }),
        "a corner of the unturned square"
    );
}

#[test]
fn a_custom_pad_draws_its_anchor_and_its_primitives() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "1" smd custom (at 10 10) (size 0.5 0.5) (layers "F.Cu") (net "GND")
               (options (clearance outline) (anchor circle))
               (primitives (gr_poly (pts (xy 1 -1) (xy 3 -1) (xy 3 1) (xy 1 1)) (width 0) (fill yes)))))"#,
    );
    let mesh = f_cu(&board);
    assert!(mesh.covers(Point { x: 10.0, y: 10.0 }), "the anchor");
    assert!(mesh.covers(Point { x: 12.0, y: 10.0 }), "the polygon");
    assert_close(
        mesh.area(),
        PI * 0.25 * 0.25 + 4.0,
        0.01,
        "anchor and polygon",
    );
}

#[test]
fn an_unplated_hole_leaves_no_copper() {
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "" np_thru_hole circle (at 0 0) (size 0.75 0.75) (drill 0.75) (layers "*.Cu" "*.Mask")))"#,
    );
    assert!(f_cu(&board).is_empty());
}

#[test]
fn a_zone_fill_is_its_polygon_with_the_spliced_hole_open() {
    // A 10 by 10 square with a 2 by 2 hole spliced in the way KiCad
    // writes it: out along a zero-width bridge, around the hole the other
    // way, and back.
    let board = board(
        r#"(zone (net "GND") (layers "F.Cu") (hatch edge 0.5) (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
             (filled_polygon (layer "F.Cu") (pts
               (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10) (xy 0 5)
               (xy 4 5) (xy 4 6) (xy 6 6) (xy 6 4) (xy 4 4) (xy 4 5)
               (xy 0 5))))"#,
    );
    let mesh = f_cu(&board);
    assert_close(mesh.area(), 100.0 - 4.0, 1e-6, "fill area");
    assert!(!mesh.covers(Point { x: 5.0, y: 5.0 }), "the hole");
    assert!(mesh.covers(Point { x: 2.0, y: 2.0 }), "the fill");
}

#[test]
fn the_outline_is_its_strokes_and_its_bounds_are_the_board() {
    // A 20 by 10 board with one rounded corner of radius 2.
    let board = board(
        r#"(gr_line (start 0 0) (end 20 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
           (gr_line (start 20 0) (end 20 8) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
           (gr_arc (start 20 8) (mid 19.414214 9.414214) (end 18 10) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
           (gr_line (start 18 10) (end 0 10) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
           (gr_line (start 0 10) (end 0 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))"#,
    );
    let tessellator = Tessellator::new(TOLERANCE);
    let mesh = tessellator.outline(&board);
    let length = 20.0 + 8.0 + PI + 18.0 + 10.0;
    // Five round-capped strokes: the caps add a full disc per stroke.
    assert_close(
        mesh.area(),
        length * 0.1 + 5.0 * PI * 0.05 * 0.05,
        0.02,
        "outline area",
    );
    let bounds = tessellator.outline_bounds(&board).expect("an outline");
    assert_bounds_eq(bounds, (0.0, 0.0), (20.0, 10.0));
}

#[test]
fn a_footprint_edge_cut_joins_the_outline_in_its_frame() {
    // A 20 by 10 board whose connector footprint, at (10, 10) turned 90
    // degrees, carries a slot edge from (0, 0) to (5, 0) in its frame: on
    // the board that runs up the screen, from (10, 10) to (10, 5).
    let with_slot = board(
        r#"(gr_rect (start 0 0) (end 20 10) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
           (footprint "J" (layer "F.Cu") (at 10 10 90)
             (fp_line (start 0 0) (end 5 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts")))"#,
    );
    let tessellator = Tessellator::new(TOLERANCE);
    let mesh = tessellator.outline(&with_slot);
    assert!(
        mesh.covers(Point { x: 10.0, y: 7.5 }),
        "the slot edge is stroked"
    );
    assert_bounds_eq(
        tessellator.outline_bounds(&with_slot).expect("an outline"),
        (0.0, 0.0),
        (20.0, 10.0),
    );
    let only_board = board(
        r#"(footprint "J" (layer "F.Cu") (at 10 10 90)
             (fp_line (start 0 0) (end 5 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts")))"#,
    );
    assert_bounds_eq(
        tessellator
            .outline_bounds(&only_board)
            .expect("the footprint's edge"),
        (10.0, 5.0),
        (10.0, 10.0),
    );
}

#[test]
fn a_custom_pad_primitive_wound_the_other_way_still_adds_copper() {
    // The primitive is listed clockwise on the screen (the opposite of
    // the generated anchor) and covers the anchor: the two must add up,
    // not cancel.
    let board = board(
        r#"(footprint "X" (layer "F.Cu") (at 0 0)
             (pad "1" smd custom (at 10 10) (size 0.5 0.5) (layers "F.Cu") (net "GND")
               (options (clearance outline) (anchor circle))
               (primitives (gr_poly (pts (xy -0.5 -0.5) (xy -0.5 0.5) (xy 0.5 0.5) (xy 0.5 -0.5)) (width 0) (fill yes)))))"#,
    );
    let mesh = f_cu(&board);
    assert!(
        mesh.covers(Point { x: 10.0, y: 10.0 }),
        "the anchor is still copper"
    );
    assert_close(mesh.area(), 1.0, 0.01, "the square, anchor inside it");
}

/// The arc's extremes are computed points, so equal to within rounding.
fn assert_bounds_eq(bounds: Bounds, min: (f64, f64), max: (f64, f64)) {
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(
        close(bounds.min.x, min.0)
            && close(bounds.min.y, min.1)
            && close(bounds.max.x, max.0)
            && close(bounds.max.y, max.1),
        "bounds {bounds:?}, expected {min:?} to {max:?}"
    );
}

#[test]
fn a_board_without_an_outline_has_no_outline_bounds() {
    let board =
        board(r#"(segment (start 10 20) (end 14 20) (width 0.5) (layer "F.Cu") (net "GND"))"#);
    assert_eq!(Tessellator::new(TOLERANCE).outline_bounds(&board), None);
}

#[test]
fn the_hand_off_board_tessellates_within_its_outline() {
    let board = hand_off_board();
    let bounds = Tessellator::new(TOLERANCE)
        .outline_bounds(board)
        .expect("the board has an outline");
    // The hand-off README: one viewBox of 111.9886 by 57.9882 mm.
    assert_close(bounds.width(), 111.9886, 1e-3, "outline width");
    assert_close(bounds.height(), 57.9882, 1e-3, "outline height");
    let mesh = f_cu(board);
    assert!(
        mesh.positions.len() > 10_000,
        "{} vertices",
        mesh.positions.len()
    );
    assert_eq!(mesh.indices.len() % 3, 0);
    let copper = mesh.bounds().expect("copper");
    assert!(
        copper.min.x >= bounds.min.x - 0.5 && copper.max.x <= bounds.max.x + 0.5,
        "copper {copper:?} inside the outline {bounds:?}"
    );
    // The F.Cu fill covers most of the board (the hand-off PNG is red
    // nearly everywhere), so the mesh area is a large share of the board.
    assert!(
        mesh.area() > 0.5 * bounds.width() * bounds.height(),
        "area {}",
        mesh.area()
    );
}
