//! Malformed input through `parse_board`: an error with a position, never
//! a panic. Truncations and number mutations of the hand-off board are the
//! fuzz-style sweep.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use kicad_core::{Error, MIN_BOARD_VERSION, parse_board};
use test_case::test_case;

const BOARD_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);

fn source() -> &'static str {
    static SOURCE: OnceLock<String> = OnceLock::new();
    SOURCE.get_or_init(|| {
        std::fs::read_to_string(BOARD_PATH).expect("the hand-off board is in the repo")
    })
}

/// A small well-formed board to mutate one field at a time.
fn tiny(version: u32, extra: &str) -> String {
    format!(
        r#"(kicad_pcb (version {version}) (generator "pcbnew")
  (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
  (segment (start 0 0) (end 1 0) (width 0.25) (layer "F.Cu") (net "GND"))
  {extra}
)"#
    )
}

fn error_of(source: &str) -> Error {
    parse_board(source).expect_err("the board is refused")
}

#[test]
fn the_tiny_board_itself_parses() {
    let board = parse_board(&tiny(MIN_BOARD_VERSION, "")).expect("parses");
    assert_eq!(board.segments.len(), 1);
    assert_eq!(board.nets.len(), 1);
}

#[test_case(20_221_018; "KiCad 7")]
#[test_case(20_240_108; "KiCad 8")]
#[test_case(20_241_228; "the day before KiCad 9")]
fn an_older_format_is_refused_naming_its_version(version: u32) {
    let error = error_of(&tiny(version, ""));
    assert_eq!(error, Error::UnsupportedVersion { version });
    let message = error.to_string();
    assert!(message.contains(&version.to_string()), "{message}");
    assert!(message.contains("save it again"), "{message}");
}

#[test]
fn the_version_is_checked_before_anything_else() {
    let error = error_of("(kicad_pcb (version 20221018) (segment))");
    assert_eq!(
        error,
        Error::UnsupportedVersion {
            version: 20_221_018
        }
    );
}

#[test_case("(kicad_sch (version 20250114))", "expected (kicad_pcb ...), found (kicad_sch ...)"; "a schematic")]
#[test_case("(kicad_pcb (generator \"x\"))", "expected (version ...) in (kicad_pcb ...)"; "no version")]
#[test_case("(kicad_pcb (version abc))", "expected value 1 of (version ...) to be an integer, found \"abc\""; "version not a number")]
#[test_case("(kicad_pcb (version -5))", "expected the format version to be a positive integer, found -5"; "negative version")]
fn a_file_that_is_not_a_board_is_refused_with_the_reason(source: &str, message: &str) {
    let error = error_of(source).to_string();
    assert!(error.ends_with(message), "{error}");
}

#[test_case(r#"(gr_line (start 0 0) (end 1 1) (stroke (width 0.1) (type default)) (layer "Rescue"))"#; "a graphic on the Rescue layer")]
#[test_case(r#"(gr_text "x" (at 0 0) (layer "Rescue") (effects (font (size 1 1))))"#; "a text on the Rescue layer")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (fp_circle (center 0 0) (end 1 0) (stroke (width 0.1) (type solid)) (fill no) (layer "Rescue")))"#; "a footprint graphic on the Rescue layer")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu" "Rescue")))"#; "a pad on the Rescue layer")]
#[test_case(r#"(zone (net "GND") (layer "Rescue") (polygon (pts (xy 0 0) (xy 1 0) (xy 1 1))))"#; "a zone on the Rescue layer")]
#[test_case(r#"(via (at 0 0) (size 0.8) (drill 0.4) (layers "F.Cu" "Rescue") (net "GND"))"#; "a via to the Rescue layer")]
fn an_unknown_layer_is_an_error_naming_it(item: &str) {
    let error = error_of(&tiny(MIN_BOARD_VERSION, item)).to_string();
    assert!(error.contains("unknown layer \"Rescue\""), "{error}");
}

#[test_case(r#"(segment (start 0 0) (end 1 0) (layer "F.Cu") (net "GND"))"#, "expected (width ...) in (segment ...)"; "segment without width")]
#[test_case(r#"(segment (start 0) (end 1 0) (width 0.25) (layer "F.Cu") (net "GND"))"#, "expected at least 2 value(s) in (start ...), found 1"; "start with one coordinate")]
#[test_case(r#"(segment (start 0 0) (end 1 0) (width 0.2.5) (layer "F.Cu") (net "GND"))"#, "expected value 1 of (width ...) to be a number, found \"0.2.5\""; "width not a number")]
#[test_case(r#"(segment (start 0 0) (end 1 0) (width 1e999) (layer "F.Cu") (net "GND"))"#, "to be a number, found \"1e999\""; "width infinite")]
#[test_case(r#"(via (at 0 0) (size 0.8) (drill 0.4) (layers "F.Cu") (net "GND"))"#, "expected at least 2 value(s) in (layers ...), found 1"; "via with one layer")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (pad "1" smd (at 0 0) (size 1 1) (layers "F.Cu")))"#, "expected value 3 of (pad ...) to be an atom"; "pad without a shape")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (pad "1" smd hexagon (at 0 0) (size 1 1) (layers "F.Cu")))"#, "expected a pad shape"; "pad with an unknown shape")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (pad "1" glued rect (at 0 0) (size 1 1) (layers "F.Cu")))"#, "expected a pad type"; "pad with an unknown type")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (at 0 0) (pad "1" smd rect (at 0 0) (size 1 1)))"#, "expected (layers ...) in (pad ...)"; "pad without layers")]
#[test_case(r#"(footprint "a:b" (layer "F.Cu") (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#, "expected (at ...) in (footprint ...)"; "footprint without a position")]
#[test_case(r#"(zone (net "GND") (layer "F.Cu"))"#, "expected (polygon ...) in (zone ...)"; "zone without an outline")]
#[test_case(r#"(zone (net "GND") (layer "F.Cu") (polygon (pts (xy 0 0) (xy 1 x))))"#, "to be a number, found \"x\""; "zone outline with a bad coordinate")]
#[test_case(r#"(gr_arc (start 0 0) (end 1 1) (stroke (width 0.1) (type default)) (layer "F.Cu"))"#, "expected (mid ...) in (gr_arc ...)"; "arc without a mid point")]
#[test_case(r#"(gr_text "x" (at 0 0 abc) (layer "F.Cu"))"#, "expected value 3 of (at ...) to be a number, found \"abc\""; "text rotation not a number")]
fn a_missing_or_malformed_field_is_an_error_naming_it(item: &str, message: &str) {
    let error = error_of(&tiny(MIN_BOARD_VERSION, item)).to_string();
    assert!(error.contains(message), "{error}");
    assert!(error.starts_with("line "), "{error}");
}

#[test_case("(net 0 \"\") (net 1 \"GND\") (via (at 0 0) (size 0.8) (drill 0.4) (layers \"F.Cu\" \"B.Cu\") (net 7))", "net 7 is not in the net table"; "net number past the table")]
#[test_case("(net 0 \"\") (via (at 0 0) (size 0.8) (drill 0.4) (layers \"F.Cu\" \"B.Cu\") (net -1))", "net -1 is not in the net table"; "negative net number")]
#[test_case("(net 1 \"GND\") (net 1 \"VCC\")", "net 1 is declared twice"; "net declared twice")]
fn a_bad_net_table_reference_is_an_error(item: &str, message: &str) {
    let error = error_of(&tiny(MIN_BOARD_VERSION, item)).to_string();
    assert!(error.contains(message), "{error}");
}

#[test_case("(layers (0 \"F.Cu\" signal) (0 \"F.Cu\" signal))", "layer \"F.Cu\" is declared twice"; "layer declared twice")]
#[test_case("(layers (0 \"F.Cu\" copper))", "expected a layer type"; "layer with an unknown type")]
#[test_case("(layers (zero \"F.Cu\" signal))", "expected a layer number, found \"zero\""; "layer with a bad number")]
#[test_case("(general (thickness 1.6))", "expected (layers ...) in (kicad_pcb ...)"; "no layer table")]
fn a_bad_layer_table_is_an_error(table: &str, message: &str) {
    let source = format!("(kicad_pcb (version {MIN_BOARD_VERSION}) {table})");
    let error = error_of(&source).to_string();
    assert!(error.contains(message), "{error}");
}

#[test]
fn unknown_elements_are_skipped() {
    let board = parse_board(&tiny(
        MIN_BOARD_VERSION,
        r#"(group "g" (members "x")) (dimension (type aligned)) (gr_text_box "t") (unknown_future_thing 1 2 3)"#,
    ))
    .expect("unknown elements do not refuse the board");
    assert_eq!(board.segments.len(), 1);
}

/// Parses `text` and reports whether it panicked; the result itself is
/// of no interest.
fn panics(text: &str) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let _ = parse_board(text);
    }))
    .is_err()
}

/// A small deterministic generator (xorshift64*) so the random set is the
/// same on every run and needs no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bound fits u64"))
            .expect("the remainder fits usize")
    }
}

/// Truncates at a byte offset, backing up to a char boundary.
fn truncated(source: &str, offset: usize) -> &str {
    let mut end = offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    source.get(..end).expect("end is a char boundary")
}

#[test]
fn truncations_at_regular_offsets_never_panic() {
    let source = source();
    // About 250 offsets over the 2 MB board; a prime stride so the cuts
    // do not line up with the file's own rhythm.
    let stride = 8_191;
    let failures: Vec<usize> = (0..source.len())
        .step_by(stride)
        .filter(|&offset| panics(truncated(source, offset)))
        .collect();
    assert!(failures.is_empty(), "panics at offsets {failures:?}");
}

#[test]
fn truncations_at_seeded_random_offsets_never_panic() {
    let source = source();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let offsets: Vec<usize> = (0..128).map(|_| rng.below(source.len())).collect();
    let failures: Vec<usize> = offsets
        .into_iter()
        .filter(|&offset| panics(truncated(source, offset)))
        .collect();
    assert!(failures.is_empty(), "panics at offsets {failures:?}");
}

#[test]
fn a_truncated_board_is_an_error_not_a_board() {
    let source = source();
    let half = truncated(source, source.len() / 2);
    let error = parse_board(half).expect_err("half a board is refused");
    assert!(error.to_string().contains("expected )"), "{error}");
}

/// Replaces every number whose ordinal is a multiple of `every` with
/// `mutant`, leaving the parentheses and strings alone.
fn mutate_numbers(source: &str, every: usize, mutant: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut token = String::new();
    let mut ordinal = 0usize;
    let flush = |token: &mut String, out: &mut String, ordinal: &mut usize| {
        if token.is_empty() {
            return;
        }
        let numeric = token.parse::<f64>().is_ok();
        if numeric {
            *ordinal += 1;
            if ordinal.is_multiple_of(every) {
                out.push_str(mutant);
                token.clear();
                return;
            }
        }
        out.push_str(token);
        token.clear();
    };
    for c in source.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                flush(&mut token, &mut out, &mut ordinal);
                in_string = true;
                out.push(c);
            }
            '(' | ')' | ' ' | '\t' | '\r' | '\n' => {
                flush(&mut token, &mut out, &mut ordinal);
                out.push(c);
            }
            other => token.push(other),
        }
    }
    flush(&mut token, &mut out, &mut ordinal);
    out
}

#[test_case("abc", 1; "every number a word")]
#[test_case("abc", 7; "every 7th number a word")]
#[test_case("abc", 1_000; "every 1000th number a word")]
#[test_case("1e999", 1; "every number infinity")]
#[test_case("1e999", 7; "every 7th number infinity")]
#[test_case("1e999", 1_000; "every 1000th number infinity")]
#[test_case("-", 1; "every number a sign")]
#[test_case("-", 7; "every 7th number a sign")]
#[test_case("-", 1_000; "every 1000th number a sign")]
#[test_case("", 1; "every number nothing")]
#[test_case("", 7; "every 7th number nothing")]
#[test_case("", 1_000; "every 1000th number nothing")]
#[test_case("1.2.3", 1; "every number two points")]
#[test_case("1.2.3", 7; "every 7th number two points")]
#[test_case("1.2.3", 1_000; "every 1000th number two points")]
#[test_case("NaN", 1; "every number NaN")]
#[test_case("NaN", 7; "every 7th number NaN")]
#[test_case("NaN", 1_000; "every 1000th number NaN")]
#[test_case("99999999999999999999999", 1; "every number a huge integer")]
#[test_case("99999999999999999999999", 7; "every 7th number a huge integer")]
#[test_case("99999999999999999999999", 1_000; "every 1000th number a huge integer")]
#[test_case("-0.0", 1; "every number negative zero, still a number")]
#[test_case("-0.0", 7; "every 7th number negative zero, still a number")]
#[test_case("-0.0", 1_000; "every 1000th number negative zero, still a number")]
fn mutated_numbers_never_panic(mutant: &str, every: usize) {
    let mutated = mutate_numbers(source(), every, mutant);
    assert!(
        !panics(&mutated),
        "panicked with every {every}th number as {mutant:?}"
    );
}

#[test]
fn a_mutated_number_in_a_required_field_is_an_error() {
    let source = source();
    let mutated = mutate_numbers(source, 1, "abc");
    let error = parse_board(&mutated).expect_err("every number is a word");
    assert!(error.to_string().contains("found \"abc\""), "{error}");
}
