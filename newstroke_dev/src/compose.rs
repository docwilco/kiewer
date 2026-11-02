//! The glyph table from the glyph map list: `charlist.txt` names, code
//! point by code point, which drawn glyph (or glyphs, with accents) stands
//! for it, and this module writes each as the string KiCad's stroke font
//! reads: two characters of metrics, then the strokes as pairs of
//! coordinate characters offset from `R`, strokes separated by ` R`.
//!
//! The rules are `fontconv.awk`'s from the Newstroke release, kept to the
//! letter so the output can be checked against the table that script
//! wrote, quirks included: a point whose two characters repeat the
//! previous point's is dropped even across a stroke break, an accent's
//! offset is read from a named anchor on both glyphs (0 when either lacks
//! it), and a name the libraries do not define draws as `DEL`.

use crate::error::{Error, Result};
use crate::legacy::{Glyph, Library};

/// The character coordinate 0 is written as.
const ORIGIN: u8 = b'R';
/// Added to every y before encoding, so the baseline sits at `[`.
const BASE: i64 = 9;
/// The glyph every undefined name and every skipped code draws as.
const MISSING: &str = "DEL";

/// What a leading character of a glyph name does to the glyph: a scale
/// of each axis and an offset of y, in font units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Transform {
    sx: i64,
    sy: i64,
    oy: i64,
}

const IDENTITY: Transform = Transform {
    sx: 1,
    sy: 1,
    oy: 0,
};

/// The prefixes the awk knew: flipped, inverted about the x-height, the
/// cap height or the symbol height, turned about those, and raised or
/// lowered for super- and subscripts.
fn transform(prefix: char) -> Option<Transform> {
    const X_HEIGHT: i64 = -14;
    const CAP_HEIGHT: i64 = -21;
    const SYM_HEIGHT: i64 = -16;
    const SUP_OFFSET: i64 = -13;
    const SUB_OFFSET: i64 = 6;
    let t = |sx, sy, oy| Some(Transform { sx, sy, oy });
    match prefix {
        '!' => t(-1, 1, 0),
        '-' => t(1, -1, X_HEIGHT),
        '=' => t(1, -1, CAP_HEIGHT),
        '~' => t(1, -1, SYM_HEIGHT),
        '+' => t(-1, -1, X_HEIGHT),
        '%' => t(-1, -1, CAP_HEIGHT),
        '*' => t(-1, -1, SYM_HEIGHT),
        '^' => t(1, 1, SUP_OFFSET),
        '`' => t(-1, 1, SUP_OFFSET),
        '.' => t(1, 1, SUB_OFFSET),
        ',' => t(-1, 1, SUB_OFFSET),
        _ => None,
    }
}

/// A glyph name split into its transform prefix and the library name.
fn split(name: &str) -> (Transform, &str) {
    let mut chars = name.chars();
    match chars.next().and_then(transform) {
        Some(transform) => (transform, chars.as_str()),
        None => (IDENTITY, name),
    }
}

/// The table: one encoded glyph per code point from `first`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// The code point of the first glyph.
    pub first: u32,
    /// The encoded glyphs, in code point order.
    pub glyphs: Vec<String>,
}

/// Builds the table, appending a line to `warnings` for every name the
/// libraries lack (drawn as `DEL`) and every anchor an accent asked for
/// that one of its glyphs lacks (placed without an offset).
///
/// # Errors
/// A directive the converter does not know, a `startchar` or `skipcodes`
/// without a whole number, a coordinate outside the printable range after
/// its offsets, or a list that never says `startchar`.
pub fn build(library: &Library, charlist: &str, warnings: &mut Vec<String>) -> Result<Table> {
    let mut composer = Composer {
        library,
        warnings,
        first: None,
        glyphs: Vec::new(),
        accent_offset_x: 0,
    };
    let mut ligature: Option<Ligature> = None;
    for (index, line) in charlist.lines().enumerate() {
        let number = index + 1;
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(&head) = fields.first() else {
            continue;
        };
        let malformed = |what: String| Error::Malformed(format!("charlist.txt:{number}: {what}"));
        match head {
            "startchar" => {
                let code = fields
                    .get(1)
                    .and_then(|f| f.parse::<u32>().ok())
                    .ok_or_else(|| malformed("startchar without a code point".to_owned()))?;
                composer.first = Some(code);
            }
            "skipcodes" => {
                let count = fields
                    .get(1)
                    .and_then(|f| f.parse::<usize>().ok())
                    .ok_or_else(|| malformed("skipcodes without a count".to_owned()))?;
                for _ in 0..count {
                    let mut glyph = composer.metric(MISSING)?;
                    glyph.push_str(&composer.graph(MISSING, 0, 0)?);
                    composer.glyphs.push(glyph);
                }
            }
            "+" | "+w" | "+p" => {
                let strokes = composer.compose(&fields, 0)?;
                let (primary, accent) = (field(&fields, 1), field(&fields, 2));
                let metric = match head {
                    "+w" => composer.metric2(primary, accent, 0, composer.accent_offset_x)?,
                    "+p" => composer.metric2(accent, primary, composer.accent_offset_x, 0)?,
                    _ => composer.metric(primary)?,
                };
                composer.glyphs.push(metric + &strokes);
            }
            "+(" => {
                let strokes = composer.compose(&fields, 0)?;
                let name = field(&fields, 1).to_owned();
                ligature = Some(Ligature {
                    first: name.clone(),
                    previous: name,
                    x: 0,
                    strokes,
                });
            }
            "+|" | "+)" => {
                let Some(mut run) = ligature.take() else {
                    return Err(malformed(format!("{head} without a +( before it")));
                };
                let name = field(&fields, 1);
                run.x += composer.distance(&run.previous, name);
                run.strokes.push_str(" R");
                run.strokes.push_str(&composer.compose(&fields, run.x)?);
                name.clone_into(&mut run.previous);
                if head == "+)" {
                    let metric = composer.metric2(&run.first, name, 0, run.x)?;
                    composer.glyphs.push(metric + &run.strokes);
                } else {
                    ligature = Some(run);
                }
            }
            // The awk's own output header, its statistics switch, its
            // comments, and comment lines.
            "font" | "stats" | "//" => {}
            _ if head.starts_with('#') => {}
            _ => return Err(malformed(format!("unknown directive {head:?}"))),
        }
    }
    let first = composer
        .first
        .ok_or_else(|| Error::Malformed("charlist.txt never says startchar".to_owned()))?;
    Ok(Table {
        first,
        glyphs: composer.glyphs,
    })
}

/// The field at `index`, or the empty string past the end: the awk's `$n`.
fn field<'a>(fields: &[&'a str], index: usize) -> &'a str {
    fields.get(index).copied().unwrap_or("")
}

/// A glyph being joined from several (`+(`, `+|`, `+)`): each next glyph
/// is placed where the previous one's right bearing meets its left.
struct Ligature {
    first: String,
    previous: String,
    x: i64,
    strokes: String,
}

struct Composer<'a> {
    library: &'a Library,
    warnings: &'a mut Vec<String>,
    first: Option<u32>,
    glyphs: Vec<String>,
    /// The x offset of the last accent placed: what `+w` and `+p` widen
    /// the metric by.
    accent_offset_x: i64,
}

impl Composer<'_> {
    /// The glyph `name` stands for, after its transform prefix: the one
    /// named, or `DEL` untransformed when the libraries lack it (the awk
    /// replaced the whole name, prefix included), with a warning when
    /// `warn` (once per drawing, as the awk commented once per drawing).
    fn resolve(&mut self, name: &str, warn: bool) -> (Transform, &Glyph) {
        let (transform, base) = split(name);
        if let Some(glyph) = self.library.glyphs.get(base) {
            return (transform, glyph);
        }
        if warn {
            self.warnings
                .push(format!("glyph {base} not found; drawn as {MISSING}"));
        }
        let glyph = self
            .library
            .glyphs
            .get(MISSING)
            .unwrap_or_else(|| panic!("the libraries define {MISSING}"));
        (IDENTITY, glyph)
    }

    /// The strokes of `name`, offset by `ox` and `oy` units.
    fn graph(&mut self, name: &str, ox: i64, oy: i64) -> Result<String> {
        let (transform, glyph) = self.resolve(name, true);
        let oy = oy + transform.oy + BASE;
        let mut out = String::new();
        let mut previous: Option<[u8; 2]> = None;
        for (index, stroke) in glyph.strokes.iter().enumerate() {
            if index > 0 {
                out.push_str(" R");
            }
            for &(x, y) in stroke {
                let pair = [
                    encode(x * transform.sx + ox)?,
                    encode(y * transform.sy + oy)?,
                ];
                if previous == Some(pair) {
                    continue;
                }
                out.push(char::from(pair[0]));
                out.push(char::from(pair[1]));
                previous = Some(pair);
            }
        }
        Ok(out)
    }

    /// The left and right bearings of `name` as two characters.
    fn metric(&mut self, name: &str) -> Result<String> {
        self.metric2(name, name, 0, 0)
    }

    /// The left bearing of `left` (moved by `left_offset`) and the right
    /// bearing of `right` (moved by `right_offset`) as two characters: the
    /// metric of a glyph widened by an accent or joined to another.
    fn metric2(
        &mut self,
        left: &str,
        right: &str,
        left_offset: i64,
        right_offset: i64,
    ) -> Result<String> {
        let (left_bearing, _) = self.bearings(left);
        let (_, right_bearing) = self.bearings(right);
        let mut out = String::new();
        out.push(char::from(encode(left_bearing + left_offset)?));
        out.push(char::from(encode(right_bearing + right_offset)?));
        Ok(out)
    }

    /// The bearings of `name` with its transform applied: a flipped glyph
    /// swaps them.
    fn bearings(&mut self, name: &str) -> (i64, i64) {
        let (transform, glyph) = self.resolve(name, false);
        if transform.sx < 0 {
            (-glyph.right, -glyph.left)
        } else {
            (glyph.left, glyph.right)
        }
    }

    /// How far right of `from`'s origin `to`'s origin sits when `to`
    /// follows `from` with their bearings touching.
    fn distance(&mut self, from: &str, to: &str) -> i64 {
        self.bearings(from).1 - self.bearings(to).0
    }

    /// Where an accent's anchor goes to meet the base glyph's: the
    /// difference of the two named anchors, or no offset when either glyph
    /// lacks its anchor. `anchor` names one point on both, or
    /// `on_base=on_accent`.
    fn accent_offset(&mut self, base: &str, accent: &str, anchor: &str) -> (i64, i64) {
        if anchor.is_empty() || anchor == "#" {
            return (0, 0);
        }
        let (base_anchor, accent_anchor) = anchor.split_once('=').unwrap_or((anchor, anchor));
        // The glyphs as named, without the DEL substitute: a missing glyph
        // has no anchors, as in the awk.
        let (base_t, base_name) = split(base);
        let (accent_t, accent_name) = split(accent);
        let point = |name: &str, anchor: &str| {
            self.library
                .glyphs
                .get(name)
                .and_then(|glyph| glyph.anchors.get(anchor).copied())
        };
        if let (Some(b), Some(a)) = (
            point(base_name, base_anchor),
            point(accent_name, accent_anchor),
        ) {
            (
                base_t.sx * b.0 - accent_t.sx * a.0,
                base_t.sy * b.1 + base_t.oy - accent_t.sy * a.1 - accent_t.oy,
            )
        } else {
            self.warnings.push(format!(
                "anchor {base_anchor} on {base_name} or {accent_anchor} on {accent_name} not found; the accent is placed without an offset"
            ));
            (0, 0)
        }
    }

    /// The strokes a `+` line composes: the glyph in field 1 at `x`, then
    /// up to three accents (fields 2, 4, 6) at their anchors (fields 3, 5,
    /// 7). An anchor of `#` ends the list.
    fn compose(&mut self, fields: &[&str], x: i64) -> Result<String> {
        let base = field(fields, 1);
        let mut out = self.graph(base, x, 0)?;
        let mut first_accent = true;
        for (accent, anchor) in [(2, 3), (4, 5), (6, 7)] {
            let accent = field(fields, accent);
            if accent.is_empty() || accent == "#" {
                break;
            }
            let anchor = field(fields, anchor);
            let (ox, oy) = self.accent_offset(base, accent, anchor);
            if first_accent {
                self.accent_offset_x = ox;
                first_accent = false;
            }
            out.push_str(" R");
            out.push_str(&self.graph(accent, x + ox, oy)?);
            if anchor == "#" {
                break;
            }
        }
        Ok(out)
    }
}

/// A coordinate as its character: `R` plus the value, which has to land on
/// a printable character for the table to be text.
fn encode(value: i64) -> Result<u8> {
    let code = i64::from(ORIGIN) + value;
    u8::try_from(code)
        .ok()
        .filter(|code| (0x20..=0x7e).contains(code))
        .ok_or_else(|| {
            Error::Malformed(format!(
                "coordinate {value} does not encode as a printable character"
            ))
        })
}

/// A table as the lines of the awk's output would show it, for a test.
#[cfg(test)]
pub fn describe(table: &Table) -> String {
    table
        .glyphs
        .iter()
        .flat_map(|glyph| [glyph.as_str(), "\n"])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library of four glyphs: A with an anchor above, an acute accent
    /// with the same anchor, DEL, and a glyph with a repeated point and
    /// named bearing pins.
    const LIBRARY: &str = "EESchema-LIBRARY Version 2.3\n\
DEF A_CAP U 0 40 Y Y 1 F N\n\
DRAW\n\
P 2 0 1 0  -250 300  250 300 N\n\
P 3 0 1 0  -350 0  0 1050  350 0 N\n\
X ABOVE ~ 0 1300 100 R 50 50 1 1 I\n\
X ~ ~ -450 0 300 U 50 50 1 1 I\n\
X ~ ~ 450 0 300 U 50 50 1 1 I\n\
ENDDRAW\n\
ENDDEF\n\
DEF ACUTE U 0 40 Y Y 1 F N\n\
DRAW\n\
P 2 0 1 0  -100 1200  100 1400 N\n\
X ABOVE ~ 0 1300 100 R 50 50 1 1 I\n\
X ~ ~ -200 0 300 U 50 50 1 1 I\n\
X ~ ~ 200 0 300 U 50 50 1 1 I\n\
ENDDRAW\n\
ENDDEF\n\
DEF DEL U 0 40 Y Y 1 F N\n\
DRAW\n\
P 5 0 1 0  -300 0  -300 1050  300 1050  300 0  -300 0 N\n\
X ~ ~ -400 0 300 U 50 50 1 1 I\n\
X ~ ~ 400 0 300 U 50 50 1 1 I\n\
ENDDRAW\n\
ENDDEF\n\
DEF DUP U 0 40 Y Y 1 F N\n\
DRAW\n\
P 3 0 1 0  0 0  0 0  100 100 N\n\
P 2 0 1 0  100 100  200 200 N\n\
X P ~ -150 0 300 U 50 50 1 1 I\n\
X S ~ 250 0 300 U 50 50 1 1 I\n\
ENDDRAW\n\
ENDDEF\n";

    /// Every directive the real list uses, once.
    const CHARLIST: &str = "# test list\n\
\n\
startchar 32\n\
font test_font\n\
// A ROW\n\
+ A_CAP\n\
+ !A_CAP\n\
+ A_CAP ACUTE ABOVE\n\
+w A_CAP ACUTE ABOVE\n\
+p ACUTE A_CAP ABOVE\n\
+ -A_CAP\n\
+ A_CAP ACUTE #\n\
skipcodes 2 0\n\
+( A_CAP\n\
+) DUP\n\
+ DUP\n\
+ ^ACUTE\n\
+ A_CAP ACUTE ABOVE=ABOVE ACUTE -\n\
+ MISSING\n";

    /// What `awk -f fontconv.awk test.lib charlist.txt` (the converter
    /// shipped in the Newstroke release, run with mawk 1.3.4) wrote for
    /// the same two inputs, string by string.
    const EXPECTED: &str = "\
I[MUWU RK[RFY[
I[WUMU RY[RFK[
I[MUWU RK[RFY[ RPCT?
IVMUWU RK[RFY[ RPCT?
IVPCT? RMUWU RK[RFY[
I[MSWS RKMRbYM
I[MUWU RK[RFY[ RPCT?
JZL[LFXFX[L[
JZL[LFXFX[L[
IcMUWU RK[RFY[ R^[`Y RbW
OWR[TY RVW
NVP6T2
I[MUWU RK[RFY[ RPCT? RPCT?
JZL[LFXFX[L[
";

    #[test]
    fn the_table_is_what_the_original_converter_wrote() {
        let mut library = Library::default();
        library.read("test.lib", LIBRARY).expect("parses");
        let mut warnings = Vec::new();
        let table = build(&library, CHARLIST, &mut warnings).expect("builds");
        assert_eq!(table.first, 32);
        assert_eq!(describe(&table), EXPECTED);
        assert_eq!(warnings, ["glyph MISSING not found; drawn as DEL"]);
    }

    #[test]
    fn an_unknown_directive_is_malformed() {
        let mut library = Library::default();
        library.read("test.lib", LIBRARY).expect("parses");
        let error = build(&library, "startchar 32\n+? A_CAP\n", &mut Vec::new())
            .expect_err("+? is not a directive");
        assert!(error.to_string().contains("charlist.txt:2"), "{error}");
    }

    #[test]
    fn a_list_without_startchar_is_malformed() {
        let mut library = Library::default();
        library.read("test.lib", LIBRARY).expect("parses");
        assert!(build(&library, "+ A_CAP\n", &mut Vec::new()).is_err());
    }
}
