//! Converts the Newstroke font sources into the glyph table file
//! `kicad_core` reads natively and the page fetches for the browser's
//! viewer (`kicad_core/src/font/newstroke.txt`).
//!
//! The sources are Vladimir Uryvaev's CC0 release of the font, in which
//! every glyph is a symbol of a KiCad legacy symbol library and
//! `charlist.txt` maps code points to glyphs and accents. The release is
//! `newstroke-font.tgz` from <http://vovanium.ru/sledy/newstroke/en>
//! (sha256 `dba1c334834e21bdbd15395dc610ecc2bf2eb6675ba0c4585597b375b7bd099e`).
//!
//! Usage, from the workspace root, with the tarball extracted into a
//! directory of its own:
//!
//! ```text
//! cargo run --bin newstroke_dev -- <extracted dir> kicad_core/src/font/newstroke.txt
//! ```
//!
//! The converter reads `symbol.lib`, `font.lib` and `charlist.txt` from
//! the directory, applies the rules of the release's `fontconv.awk`
//! ([`compose`]) and writes the table ([`emit`]): one glyph a line from
//! U+0020, nothing else, so the file carries no header and its origin is
//! said here and in `kicad_core::font`. Names the libraries lack and
//! anchors an accent asks for that a glyph lacks are reported on stderr,
//! as the awk reported them in comments; the release has 27 of the
//! first and 49 of the second, and the table it shipped has the same.

mod compose;
mod emit;
mod error;
mod legacy;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::error::{Error, Result};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code().code())
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [source_dir, output] = args.as_slice() else {
        return Err(Error::Usage(
            "usage: newstroke_dev <extracted newstroke-font.tgz> <output .txt>".to_owned(),
        ));
    };
    let source_dir = Path::new(source_dir);
    let output = PathBuf::from(output);
    let mut library = legacy::Library::default();
    for name in ["symbol.lib", "font.lib"] {
        library.read(name, &read(&source_dir.join(name))?)?;
    }
    let charlist = read(&source_dir.join("charlist.txt"))?;
    let mut warnings = Vec::new();
    let table = compose::build(&library, &charlist, &mut warnings)?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let text = emit::table_text(&table)?;
    std::fs::write(&output, text).map_err(|source| Error::Write {
        path: output.clone(),
        source,
    })?;
    eprintln!(
        "wrote {} glyphs from U+{:04X} to {}",
        table.glyphs.len(),
        table.first,
        output.display()
    );
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    // The libraries are ASCII but for a few bytes in symbol.lib's comments.
    let bytes = std::fs::read(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
