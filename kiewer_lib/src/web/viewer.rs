//! The viewer page and what it loads: the wasm-bindgen output of
//! `kiewer_viewer` (embedded at build time, see `build.rs`) and the
//! hand-off board at a fixed URL. Uploads (#16) replace the fixed board;
//! content-hashed names and pre-compression for the assets are #26.

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

#[derive(Template)]
#[template(path = "viewer.html")]
struct Page {
    build: String,
    embedded: bool,
    board_path: &'static str,
}

async fn page() -> Result<Html<String>> {
    let page = Page {
        build: crate::build::line(),
        embedded: embedded(),
        board_path: BOARD_PATH,
    };
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
