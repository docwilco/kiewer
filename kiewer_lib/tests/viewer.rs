//! The viewer page and its assets through a real listener. The wasm and
//! its script are in the binary only after `scripts/build-viewer.sh` has
//! run (`kiewer_lib/build.rs`), so their routes are specified both ways:
//! the bytes with their types when embedded, a 503 naming the script when
//! not. `embedded()` says which this build is.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::auth::Auth;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::AppState;
use kiewer_lib::web::viewer::{BOARD_PATH, NOT_BUILT, embedded};
use kiewer_test_support::{TempDir, http, scratch_dir};

const BOARD_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);

async fn start() -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = AppState {
        db: kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    (http::serve_on_free_port(state).await, dir)
}

#[tokio::test]
async fn the_viewer_page_has_the_canvas_and_loads_the_viewer_and_the_board() {
    let (addr, _dir) = start().await;
    let response = http::request(addr, "GET", "/viewer", &[], "").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        response
            .header("content-type")
            .is_some_and(|t| t.starts_with("text/html")),
        "{:?}",
        response.headers
    );
    let body = &response.body;
    assert!(body.contains("<canvas"), "{body}");
    assert!(
        body.contains("touch-action: none"),
        "two-finger pinch needs it: {body}"
    );
    assert!(body.contains("/viewer/kiewer_viewer.js"), "{body}");
    assert!(body.contains(BOARD_PATH), "{body}");
    assert!(
        body.contains("passive: false"),
        "the wheel listener prevents the page scroll: {body}"
    );
    assert!(body.contains("preventDefault"), "{body}");
    assert_eq!(
        body.contains(NOT_BUILT.trim_end()),
        !embedded(),
        "the page says when the viewer is not built in"
    );
}

#[tokio::test]
async fn the_board_is_served_as_text_from_the_repo_file() {
    let (addr, _dir) = start().await;
    let response = http::request(addr, "GET", BOARD_PATH, &[], "").await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert!(
        response.body.starts_with("(kicad_pcb"),
        "{}",
        &response.body[..40]
    );
    let file = std::fs::read_to_string(BOARD_FILE).expect("the hand-off board is in the repo");
    assert_eq!(response.body.len(), file.len());
}

#[tokio::test]
async fn the_script_and_the_wasm_are_served_when_built_in_and_refused_with_the_reason_when_not() {
    let (addr, _dir) = start().await;
    let script = http::request(addr, "GET", "/viewer/kiewer_viewer.js", &[], "").await;
    let wasm = http::request(addr, "GET", "/viewer/kiewer_viewer_bg.wasm", &[], "").await;
    if embedded() {
        assert_eq!(script.status, 200);
        assert_eq!(
            script.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert!(
            script.body.contains("export class Viewer"),
            "wasm-bindgen's module"
        );
        assert_eq!(wasm.status, 200);
        assert_eq!(wasm.header("content-type"), Some("application/wasm"));
        assert!(wasm.body.starts_with("\0asm"), "the wasm magic");
    } else {
        for response in [&script, &wasm] {
            assert_eq!(response.status, 503);
            assert_eq!(response.body, NOT_BUILT);
        }
    }
}

#[tokio::test]
async fn other_viewer_paths_are_not_found() {
    let (addr, _dir) = start().await;
    let (status, _) = http::get(addr, "/viewer/other.wasm").await;
    assert_eq!(status, 404);
}
