//! The viewer page and what it loads: the wasm-bindgen output of
//! `kiewer_viewer` (embedded at build time, see `build.rs`) and the board.
//! At `/viewer` the board is the hand-off one at a fixed URL, kept until
//! a design exists; a design page (`web/design.rs`) renders the same
//! [`Page`] with the revision's board blob and a [`DesignView`].
//! Content-hashed names and pre-compression for the assets are #26.

use askama::Template;
use axum::Router;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use super::AppState;
use crate::error::Result;

/// The viewer's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/viewer", get(page))
        .route("/viewer/kiewer_viewer.js", get(script))
        .route("/viewer/kiewer_viewer_bg.wasm", get(wasm))
        .route(BOARD_PATH, get(board))
}

/// Where the page fetches the board from.
pub const BOARD_PATH: &str = "/viewer/midi8knob.kicad_pcb";

/// The hand-off board, in the binary: the one design the tracer shows.
const BOARD: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_pcb");

/// The wasm-bindgen output, when the build found it.
#[cfg(kiewer_viewer_embedded)]
const SCRIPT: Option<&[u8]> = Some(include_bytes!(env!("KIEWER_VIEWER_JS")));
#[cfg(kiewer_viewer_embedded)]
const WASM: Option<&[u8]> = Some(include_bytes!(env!("KIEWER_VIEWER_WASM")));
#[cfg(not(kiewer_viewer_embedded))]
const SCRIPT: Option<&[u8]> = None;
#[cfg(not(kiewer_viewer_embedded))]
const WASM: Option<&[u8]> = None;

/// Whether this build carries the viewer. `false` on a tree where
/// `scripts/build-viewer.sh` has not run: the page then says so and the
/// asset routes answer 503.
#[must_use]
pub fn embedded() -> bool {
    SCRIPT.is_some() && WASM.is_some()
}

/// What the asset routes say when the viewer is not in the binary.
pub const NOT_BUILT: &str =
    "the viewer is not built into this server: run scripts/build-viewer.sh, then build again\n";

/// One file of the revision a design page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    /// The path in the tree.
    pub path: String,
    /// The file name alone, for the download attribute.
    pub name: String,
    /// Where the bytes are: the blob route.
    pub url: String,
    /// Uncompressed bytes.
    pub size: u64,
}

/// What a design page says about the revision it shows, folded under the
/// stage. Labels and values are text already: the template knows nothing
/// of the metadata's shape, so a new field is a new pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignView {
    pub handle: String,
    pub slug: String,
    pub branch: String,
    /// The revision's hash, 64 hex characters.
    pub revision: String,
    /// Its short form, the first 8.
    pub short: String,
    /// The handle of the account that uploaded it.
    pub author: String,
    /// When, UTC, ISO 8601 to the second.
    pub created_at: String,
    pub message: Option<String>,
    /// The parsed metadata as label and value pairs.
    pub details: Vec<(&'static str, String)>,
    pub files: Vec<FileView>,
}

/// The viewer page: the hand-off board at `/viewer`, a design's revision
/// at `/{handle}/{slug}` (`web/design.rs`).
#[derive(Template)]
#[template(path = "viewer.html")]
pub struct Page {
    pub build: String,
    pub embedded: bool,
    pub not_built: &'static str,
    /// What the tab and the header name.
    pub title: String,
    /// Where the page fetches the board from, or `None` for a revision
    /// without a board.
    pub board_url: Option<String>,
    /// The design and revision shown, or `None` for the hand-off board.
    pub design: Option<DesignView>,
}

impl Page {
    /// A page for `title` with the viewer's build state filled in.
    #[must_use]
    pub fn new(title: String, board_url: Option<String>, design: Option<DesignView>) -> Self {
        Self {
            build: crate::build::line(),
            embedded: embedded(),
            not_built: NOT_BUILT.trim_end(),
            title,
            board_url,
            design,
        }
    }
}

async fn page() -> Result<Html<String>> {
    let page = Page::new("midi8knob".to_owned(), Some(BOARD_PATH.to_owned()), None);
    Ok(Html(page.render()?))
}

async fn board() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], BOARD)
}

async fn script() -> Response {
    asset(SCRIPT, "text/javascript; charset=utf-8")
}

async fn wasm() -> Response {
    // application/wasm is what WebAssembly.instantiateStreaming insists on.
    asset(WASM, "application/wasm")
}

fn asset(bytes: Option<&'static [u8]>, content_type: &'static str) -> Response {
    match bytes {
        Some(bytes) => ([(CONTENT_TYPE, content_type)], bytes).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response(),
    }
}
