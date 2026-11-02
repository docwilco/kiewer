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
}

/// The crate's result type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

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
}
