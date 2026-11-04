//! A parse with a deadline (#43): past it, the hand-off files are refused
//! with [`Error::DeadlinePassed`] before the reader gets far; within it,
//! the model is the one the parse without a deadline makes.

use std::time::{Duration, Instant};

use kicad_core::{Error, parse_board, parse_board_before, parse_schematic, parse_schematic_before};

const BOARD: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_sch");

/// A deadline no test run reaches.
fn far_off() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

#[test]
fn a_board_past_its_deadline_is_refused() {
    let error = parse_board_before(BOARD, Instant::now()).expect_err("the deadline has passed");
    assert_eq!(error, Error::DeadlinePassed);
}

#[test]
fn a_schematic_past_its_deadline_is_refused() {
    let error =
        parse_schematic_before(SCHEMATIC, Instant::now()).expect_err("the deadline has passed");
    assert_eq!(error, Error::DeadlinePassed);
}

#[test]
fn a_board_within_its_deadline_parses_as_without_one() {
    let within = parse_board_before(BOARD, far_off()).expect("parses in time");
    assert_eq!(within, parse_board(BOARD).expect("parses"));
}

#[test]
fn a_schematic_within_its_deadline_parses_as_without_one() {
    let within = parse_schematic_before(SCHEMATIC, far_off()).expect("parses in time");
    assert_eq!(within, parse_schematic(SCHEMATIC).expect("parses"));
}

#[test]
fn a_malformed_board_within_its_deadline_names_what_is_wrong() {
    let error = parse_board_before("(kicad_pcb (version 20241229)", far_off())
        .expect_err("the list is not closed");
    assert!(matches!(error, Error::Malformed { .. }), "{error}");
}
