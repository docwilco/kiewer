//! The glyph libraries: KiCad's legacy symbol library format
//! (`EESchema-LIBRARY Version 2.3`), in which Newstroke was drawn. Each
//! `DEF ... ENDDEF` is one glyph; its `P` lines are the strokes and its
//! `X` lines (pins) are the metrics and the anchors accents attach to, the
//! way `fontconv.awk` in the Newstroke release reads them.
//!
//! Coordinates in the file are in mils; a font unit is 50 of them (21
//! units to the em), and every coordinate in the sources is a whole number
//! of units. The y axis is flipped on the way in: up in the file, down in
//! the font.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Mils per font unit.
const UNIT: i64 = 50;

/// One glyph as drawn: strokes in font units, y down.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Glyph {
    /// The polylines, each a run of distinct consecutive points.
    pub strokes: Vec<Vec<(i64, i64)>>,
    /// The left bearing: the `~` pin at or left of the origin, or the pin
    /// named `P`. 0 when the glyph has neither.
    pub left: i64,
    /// The right bearing: the `~` pin right of the origin, or the pin
    /// named `S`. 0 when the glyph has neither.
    pub right: i64,
    /// Named points accents attach to: every pin by name (a later pin of
    /// the same name wins), plus `-` for the origin.
    pub anchors: BTreeMap<String, (i64, i64)>,
}

/// Every glyph of every library read, by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Library {
    pub glyphs: BTreeMap<String, Glyph>,
}

impl Library {
    /// Add the glyphs of one library file. A glyph named twice draws as
    /// the later definition, with the earlier one's anchors and bearings
    /// where the later has no pin of that name.
    ///
    /// # Errors
    /// A `P` or `X` line with too few fields or a coordinate that is not a
    /// whole number of mils in a multiple of 50, named with `file` and
    /// its line number.
    pub fn read(&mut self, file: &str, source: &str) -> Result<()> {
        let mut current: Option<(String, Glyph)> = None;
        for (index, line) in source.lines().enumerate() {
            let number = index + 1;
            let fields: Vec<&str> = line.split_whitespace().collect();
            let Some(&head) = fields.first() else {
                continue;
            };
            let malformed = |what: &str| Error::Malformed(format!("{file}:{number}: {what}"));
            match head {
                "DEF" => {
                    let name = fields
                        .get(1)
                        .ok_or_else(|| malformed("DEF without a name"))?;
                    // A redefinition draws anew but keeps the anchors and
                    // bearings of the earlier one until its own pins
                    // replace them: the awk kept them in arrays by name
                    // that nothing cleared, and the shipped table depends
                    // on it (SLASH_SMALL is in both libraries; the second
                    // has no CENTER pin and U+2298 attaches to it).
                    let mut glyph = self.glyphs.get(*name).cloned().unwrap_or_default();
                    glyph.strokes.clear();
                    glyph.anchors.insert("-".to_owned(), (0, 0));
                    current = Some(((*name).to_owned(), glyph));
                }
                "P" => {
                    let Some((_, glyph)) = current.as_mut() else {
                        return Err(malformed("P outside a DEF"));
                    };
                    let count = integer(fields.get(1).copied(), || malformed("P without a count"))?;
                    let count =
                        usize::try_from(count).map_err(|_| malformed("P with a negative count"))?;
                    let mut stroke: Vec<(i64, i64)> = Vec::new();
                    for i in 0..count {
                        let x = unit(fields.get(5 + 2 * i).copied(), || {
                            malformed("P with fewer points than its count")
                        })?;
                        let y = -unit(fields.get(6 + 2 * i).copied(), || {
                            malformed("P with fewer points than its count")
                        })?;
                        if stroke.last() != Some(&(x, y)) {
                            stroke.push((x, y));
                        }
                    }
                    glyph.strokes.push(stroke);
                }
                "X" => {
                    let Some((_, glyph)) = current.as_mut() else {
                        return Err(malformed("X outside a DEF"));
                    };
                    let pin = fields.get(1).ok_or_else(|| malformed("X without a name"))?;
                    let x = unit(fields.get(3).copied(), || malformed("X without a position"))?;
                    let y = -unit(fields.get(4).copied(), || malformed("X without a position"))?;
                    glyph.anchors.insert((*pin).to_owned(), (x, y));
                    if (x > 0 && *pin == "~") || *pin == "S" {
                        glyph.right = x;
                    } else if (x <= 0 && *pin == "~") || *pin == "P" {
                        glyph.left = x;
                    }
                }
                "ENDDEF" => {
                    if let Some((name, glyph)) = current.take() {
                        self.glyphs.insert(name, glyph);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// A whole number.
fn integer(field: Option<&str>, missing: impl FnOnce() -> Error) -> Result<i64> {
    let text = field.ok_or_else(missing)?;
    text.parse::<i64>()
        .map_err(|_| Error::Malformed(format!("expected a whole number, found {text:?}")))
}

/// A coordinate in mils, as font units.
fn unit(field: Option<&str>, missing: impl FnOnce() -> Error) -> Result<i64> {
    let mils = integer(field, missing)?;
    if mils % UNIT != 0 {
        return Err(Error::Malformed(format!(
            "coordinate {mils} is not a multiple of {UNIT} mils"
        )));
    }
    Ok(mils / UNIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_GLYPH: &str = "EESchema-LIBRARY Version 2.3\n\
DEF A_CAP U 0 40 Y Y 1 F N\n\
F0 \"U\" 0 -250 60 H V C CNN\n\
DRAW\n\
P 2 0 1 0  -250 300  250 300 N\n\
P 4 0 1 0  -350 0  0 1050  0 1050  350 0 N\n\
X ABOVE ~ 0 1300 100 R 50 50 1 1 I\n\
X ~ ~ -450 0 300 U 50 50 1 1 I\n\
X ~ ~ 450 0 300 U 50 50 1 1 I\n\
ENDDRAW\n\
ENDDEF\n";

    #[test]
    fn a_glyph_has_its_strokes_metrics_and_anchors_in_font_units() {
        let mut library = Library::default();
        library.read("test.lib", ONE_GLYPH).expect("parses");
        let glyph = library.glyphs.get("A_CAP").expect("A_CAP");
        // y is flipped: 300 mils up is -6 units.
        assert_eq!(
            glyph.strokes,
            vec![vec![(-5, -6), (5, -6)], vec![(-7, 0), (0, -21), (7, 0)]]
        );
        assert_eq!((glyph.left, glyph.right), (-9, 9));
        assert_eq!(glyph.anchors.get("ABOVE"), Some(&(0, -26)));
        assert_eq!(glyph.anchors.get("-"), Some(&(0, 0)));
        // The last unnamed pin is the one `~` names.
        assert_eq!(glyph.anchors.get("~"), Some(&(9, 0)));
    }

    #[test]
    fn a_redefinition_keeps_the_anchors_the_new_pins_do_not_name() {
        let mut library = Library::default();
        library.read("first.lib", ONE_GLYPH).expect("parses");
        library
            .read(
                "second.lib",
                "DEF A_CAP U 0 40 Y Y 1 F N\nP 2 0 1 0  0 0  0 500 N\nX ~ ~ -400 0 300 U 50 50 1 1 I\nENDDEF\n",
            )
            .expect("parses");
        let glyph = library.glyphs.get("A_CAP").expect("A_CAP");
        assert_eq!(glyph.strokes, vec![vec![(0, 0), (0, -10)]]);
        assert_eq!((glyph.left, glyph.right), (-8, 9));
        assert_eq!(glyph.anchors.get("ABOVE"), Some(&(0, -26)));
        assert_eq!(glyph.anchors.get("~"), Some(&(-8, 0)));
    }

    #[test]
    fn a_coordinate_off_the_grid_is_malformed() {
        let mut library = Library::default();
        let error = library
            .read(
                "test.lib",
                "DEF X U 0 40 Y Y 1 F N\nP 2 0 1 0  -250 300  251 300 N\nENDDEF\n",
            )
            .expect_err("251 is not a multiple of 50");
        assert!(error.to_string().contains("251"), "{error}");
    }
}
