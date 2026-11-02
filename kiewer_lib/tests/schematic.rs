//! The schematic page through a real listener: the hand-off sheet at
//! `/schematic`, its file at a fixed URL, and a design's root sheet at
//! `/{handle}/{slug}/schematic`, which is 404 until a design holds one.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::auth::Auth;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::AppState;
use kiewer_lib::web::schematic::SCHEMATIC_PATH;
use kiewer_lib::web::viewer::{NOT_BUILT, embedded};
use kiewer_test_support::{TempDir, http, scratch_dir};

const SCHEMATIC_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
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
async fn the_schematic_page_has_the_canvas_and_loads_the_viewer_and_the_sheet() {
    let (addr, _dir) = start().await;
    let response = http::request(addr, "GET", "/schematic", &[], "").await;
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
    assert!(body.contains("touch-action: none"), "{body}");
    assert!(body.contains("/viewer/kiewer_viewer.js"), "{body}");
    assert!(body.contains(SCHEMATIC_PATH), "{body}");
    assert!(body.contains("load_schematic"), "{body}");
    assert!(body.contains("#f5f4ef"), "KiCad's paper colour: {body}");
    assert!(body.contains("passive: false"), "{body}");
    assert!(
        body.contains("/api/beacon"),
        "the beacon is sent here too: {body}"
    );
    assert_eq!(
        body.contains(NOT_BUILT.trim_end()),
        !embedded(),
        "the page says when the viewer is not built in"
    );
}

#[tokio::test]
async fn the_sheet_is_served_as_text_from_the_repo_file() {
    let (addr, _dir) = start().await;
    let response = http::request(addr, "GET", SCHEMATIC_PATH, &[], "").await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert!(
        response.body.starts_with("(kicad_sch"),
        "{}",
        &response.body[..40]
    );
    let file =
        std::fs::read_to_string(SCHEMATIC_FILE).expect("the hand-off schematic is in the repo");
    assert_eq!(response.body.len(), file.len());
}

#[tokio::test]
async fn a_design_path_that_is_no_handle_and_slug_is_not_found_before_any_lookup() {
    // The pool reaches nothing: a 404 here proves the name check comes
    // first, as on the design page.
    let (addr, _dir) = start().await;
    let (status, body) = http::get(addr, "/Not%20A%20Handle/x/schematic").await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("no such schematic"), "{body}");
}
