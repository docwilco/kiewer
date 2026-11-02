//! `kiewer_render <board.kicad_pcb> <out-dir>`: the viewer's frames of a
//! board as the hand-off's pages (`viewer-drop/exports/png`), rendered
//! headless on Vulkan (lavapipe on a machine without a GPU) and written
//! as PNG files into the directory, one per view plus the overview. A
//! thin wrapper over `kiewer_viewer::headless`, which the golden-image
//! test calls the same way. Every non-zero exit is an `Exit` code of the
//! viewer's one error type; 2 is a usage error.

use std::path::Path;
use std::process::ExitCode;

use kiewer_viewer::error::{Error, Result};
use kiewer_viewer::headless::{Headless, hand_off};
use kiewer_viewer::scene::Scene;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [board, out] = args.as_slice() else {
        eprintln!("usage: kiewer_render <board.kicad_pcb> <out-dir>");
        return ExitCode::from(2);
    };
    match run(Path::new(board), Path::new(out)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code().code())
        }
    }
}

/// Parse the board at `board`, render every page and write each under
/// `out`, naming each file on stdout as it lands.
fn run(board: &Path, out: &Path) -> Result<()> {
    let text = std::fs::read_to_string(board).map_err(|source| Error::Read {
        path: board.to_owned(),
        source,
    })?;
    let parsed = kicad_core::parse_board(&text)?;
    let scene = Scene::build(&parsed).ok_or(Error::NothingToDraw)?;
    let mut headless = Headless::new()?;
    let adapter = headless.adapter();
    eprintln!(
        "kiewer_render: {} ({:?}), {} samples per pixel",
        adapter.name,
        adapter.device_type,
        headless.sample_count()
    );
    let stem = board.file_stem().map_or_else(
        || "board".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    std::fs::create_dir_all(out).map_err(|source| Error::Write {
        path: out.to_owned(),
        source,
    })?;
    for rendered in hand_off::render_all(&mut headless, &scene, &stem)? {
        let path = out.join(&rendered.name);
        std::fs::write(&path, rendered.image.to_png()?).map_err(|source| Error::Write {
            path: path.clone(),
            source,
        })?;
        println!("{}", path.display());
    }
    Ok(())
}
