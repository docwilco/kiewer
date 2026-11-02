//! The one error type of the crate: why a viewer could not be made, could
//! not load a board, or could not draw. At the JavaScript boundary each
//! becomes a `JsError` carrying its `Display` text, which the page shows.

use derive_more::{Display, Error, From};

use crate::render::Backend;

/// Every way the viewer fails. Variants are failure kinds with context,
/// never one per call site.
#[derive(Debug, Display, Error, From)]
pub enum Error {
    /// Neither WebGPU nor WebGL2 gave an adapter; `reasons` says why each
    /// did not, in the order tried.
    #[display("no GPU: {}", reasons.join("; "))]
    #[from(ignore)]
    NoAdapter {
        #[error(not(source))]
        reasons: Vec<String>,
    },
    /// The canvas gave no surface for the backend.
    #[display("{backend}: no surface on the canvas: {source}")]
    #[from(ignore)]
    Surface {
        backend: Backend,
        source: wgpu::CreateSurfaceError,
    },
    /// The adapter gave no device at the WebGL2 limits.
    #[display("{backend}: no device: {source}")]
    #[from(ignore)]
    Device {
        backend: Backend,
        source: wgpu::RequestDeviceError,
    },
    /// The adapter has no configuration for the canvas's surface.
    #[display("{backend}: the adapter does not draw to this canvas")]
    #[from(ignore)]
    NoSurfaceConfig { backend: Backend },
    /// The board text did not parse.
    #[display("the board: {_0}")]
    Board(kicad_core::Error),
    /// The board parsed but has no outline and no front copper.
    #[display("the board has no outline and no front copper to draw")]
    NothingToDraw,
    /// The schematic text did not parse.
    #[display("the schematic: {_0}")]
    #[from(ignore)]
    Schematic(kicad_core::Error),
    /// The sheet parsed but draws nothing.
    #[display("the sheet has nothing to draw")]
    EmptySheet,
    /// The surface refused to give a frame for a reason that is not a
    /// resize: a validation failure in the configuration.
    #[display("{backend}: the surface refused a frame")]
    #[from(ignore)]
    FrameRefused { backend: Backend },
    /// A headless frame larger than the adapter's textures go.
    #[cfg(not(target_arch = "wasm32"))]
    #[display(
        "a frame of {width} by {height} pixels is larger than the adapter's limit of {limit} on a side"
    )]
    #[from(ignore)]
    FrameTooLarge { width: u32, height: u32, limit: u32 },
    /// The device refused to map the headless readback buffer.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("reading the frame back: {_0}")]
    Readback(wgpu::BufferAsyncError),
    /// The mapped readback buffer could not be read.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("reading the frame back: {_0}")]
    MappedRange(wgpu::MapRangeError),
    /// The PNG encoder refused the pixels.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("encoding the PNG: {_0}")]
    PngEncode(png::EncodingError),
    /// The bytes are not a PNG.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("decoding the PNG: {_0}")]
    PngDecode(png::DecodingError),
    /// A PNG in a colour type or depth the reader does not take.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("the PNG is {_0:?}, not 8-bit RGB or RGBA")]
    #[from(ignore)]
    PngColorType(#[error(not(source))] png::ColorType),
    /// A file the headless renderer was given could not be read.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("reading {}: {source}", path.display())]
    #[from(ignore)]
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// A file the headless renderer writes could not be written.
    #[cfg(not(target_arch = "wasm32"))]
    #[display("writing {}: {source}", path.display())]
    #[from(ignore)]
    Write {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

/// The crate's result type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// The process exit code of `kiewer_render` for each kind of failure.
/// Explicit discriminants from 3, so a duplicate is a compile error; 0,
/// 1, 2, 101 and 126 up are never assigned ([`Exit::reserved`]).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    /// Reading the board or writing a PNG.
    Io = 3,
    /// The board does not parse or has nothing to draw.
    Board = 4,
    /// No Vulkan adapter, no device, or a frame that would not read back.
    Gpu = 5,
    /// A PNG that would not encode or decode.
    Png = 6,
}

#[cfg(not(target_arch = "wasm32"))]
impl Exit {
    /// Every kind, in code order.
    pub const ALL: [Exit; 4] = [Exit::Io, Exit::Board, Exit::Gpu, Exit::Png];

    /// The codes never assigned: 0 (success), 1 and 2 (the shell's and a
    /// usage error), 101 (a Rust panic) and 126 up (the shell's own).
    #[must_use]
    pub fn reserved(code: u8) -> bool {
        matches!(code, 0..=2 | 101 | 126..)
    }

    /// The code as a process exit status.
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// What each kind means, for the table.
    #[must_use]
    pub fn meaning(self) -> &'static str {
        match self {
            Exit::Io => "reading the board or writing a PNG",
            Exit::Board => "the board does not parse or has nothing to draw",
            Exit::Gpu => "no Vulkan adapter, no device, or a frame that would not read back",
            Exit::Png => "a PNG that would not encode or decode",
        }
    }

    /// The exit codes as a markdown table, the block README.md carries; a
    /// test keeps the two equal.
    #[must_use]
    pub fn table() -> String {
        use std::fmt::Write as _;
        let mut out = String::from("| Code | Kind | Meaning |\n|---|---|---|\n");
        for exit in Exit::ALL {
            // Writing to a String cannot fail.
            let _ = writeln!(out, "| {} | {exit:?} | {} |", exit.code(), exit.meaning());
        }
        out
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Error {
    /// The exit code for this failure. One `match` with no wildcard: a new
    /// variant does not compile until it has a kind. The browser's kinds
    /// have one too, since nothing stops the headless path meeting them.
    #[must_use]
    pub fn exit_code(&self) -> Exit {
        match self {
            Error::Read { .. } | Error::Write { .. } => Exit::Io,
            Error::Board(_) | Error::NothingToDraw | Error::Schematic(_) | Error::EmptySheet => {
                Exit::Board
            }
            Error::NoAdapter { .. }
            | Error::Surface { .. }
            | Error::Device { .. }
            | Error::NoSurfaceConfig { .. }
            | Error::FrameRefused { .. }
            | Error::FrameTooLarge { .. }
            | Error::Readback(_)
            | Error::MappedRange(_) => Exit::Gpu,
            Error::PngEncode(_) | Error::PngDecode(_) | Error::PngColorType(_) => Exit::Png,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_messages_name_the_backend_and_the_reasons() {
        let error = Error::NoAdapter {
            reasons: vec![
                "WebGPU: navigator.gpu is not available".to_owned(),
                "WebGL2: no adapter".to_owned(),
            ],
        };
        assert_eq!(
            error.to_string(),
            "no GPU: WebGPU: navigator.gpu is not available; WebGL2: no adapter"
        );
        let error = Error::FrameRefused {
            backend: Backend::WebGl2,
        };
        assert_eq!(error.to_string(), "webgl2: the surface refused a frame");
        let error = Error::from(
            kicad_core::parse_board("(kicad_pcb (version 20200101))").expect_err("too old"),
        );
        assert!(
            error
                .to_string()
                .starts_with("the board: board format version")
        );
    }

    #[test]
    fn no_exit_code_is_reserved_and_all_lists_every_kind_once_in_order() {
        let mut codes: Vec<u8> = Exit::ALL.iter().map(|exit| exit.code()).collect();
        for code in &codes {
            assert!(!Exit::reserved(*code), "{code} is reserved");
        }
        codes.dedup();
        assert_eq!(codes.len(), Exit::ALL.len());
        assert!(
            codes.windows(2).all(|pair| pair[0] < pair[1]),
            "in code order"
        );
    }

    #[test]
    fn the_readme_carries_the_table() {
        let readme = include_str!("../../README.md");
        assert!(
            readme.contains(&Exit::table()),
            "README.md's kiewer_render exit-code table is out of date; it should read:\n{}",
            Exit::table()
        );
    }

    #[test]
    fn each_kind_of_failure_has_its_exit_code() {
        assert_eq!(Error::NothingToDraw.exit_code(), Exit::Board);
        assert_eq!(Error::NoAdapter { reasons: vec![] }.exit_code(), Exit::Gpu);
        assert_eq!(
            Error::Readback(wgpu::BufferAsyncError).exit_code(),
            Exit::Gpu
        );
        assert_eq!(
            Error::PngColorType(png::ColorType::Grayscale).exit_code(),
            Exit::Png
        );
        let read = Error::Read {
            path: "x.kicad_pcb".into(),
            source: std::io::Error::other("gone"),
        };
        assert_eq!(read.exit_code(), Exit::Io);
        assert_eq!(read.to_string(), "reading x.kicad_pcb: gone");
    }
}
