//! The generated data file: the table as text, one encoded glyph per
//! line from U+0020, which `kicad_core::font` reads natively and the page
//! fetches for the browser's viewer.

use crate::compose::Table;
use crate::error::{Error, Result};

/// The code point the file's first line is: the space, where KiCad's
/// stroke font starts its table and where `kicad_core::font` counts from
/// (its own `FIRST`; the converter does not depend on `kicad_core`, so
/// the two are kept equal by hand).
const FIRST: u32 = 0x20;

/// The text of the glyph table file: each glyph on a line of its own, in
/// code point order from U+0020, each line ended by a newline.
///
/// # Errors
/// A table that starts anywhere but U+0020, which the file cannot say:
/// its first line is the space's.
pub fn table_text(table: &Table) -> Result<String> {
    if table.first != FIRST {
        return Err(Error::Malformed(format!(
            "the table starts at U+{:04X}; the file's first line is U+{FIRST:04X}'s",
            table.first
        )));
    }
    let mut out = String::with_capacity(table.glyphs.iter().map(|glyph| glyph.len() + 1).sum());
    for glyph in &table.glyphs {
        out.push_str(glyph);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_one_glyph_a_line_as_written() {
        let table = Table {
            first: 0x20,
            glyphs: vec![
                "JZ".to_owned(),
                "H\\K\"".to_owned(),
                "MWRYSZ R[QZ".to_owned(),
            ],
        };
        assert_eq!(
            table_text(&table).expect("a table from the space"),
            "JZ\nH\\K\"\nMWRYSZ R[QZ\n"
        );
    }

    #[test]
    fn a_table_that_starts_past_the_space_is_refused() {
        let table = Table {
            first: 0x21,
            glyphs: vec!["JZ".to_owned()],
        };
        let error = table_text(&table).expect_err("the file cannot say where it starts");
        assert_eq!(
            error.to_string(),
            "the table starts at U+0021; the file's first line is U+0020's"
        );
    }
}
