//! The viewer page and its assets through a real listener. The wasm and
//! its script are in the binary only after `scripts/build-viewer.sh` has
//! run (`kiewer_lib/build.rs`), so their routes are specified both ways:
//! the bytes with their types when embedded, under their hashed names in
//! the coding the request takes (#26), a 503 naming the script when not.
//! `embedded()` says which this build is.

use std::io::Read as _;
use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::Policy;
use kiewer_lib::web::viewer::{BOARD_PATH, NOT_BUILT, embedded, script_url, wasm_url};
use kiewer_test_support::{TempDir, http, scratch_dir};
use test_case::test_case;

const BOARD_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_pcb"
);

async fn start() -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
    );
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
    assert!(
        body.contains(&format!("from \"{}\"", script_url())),
        "the script is imported by its hashed URL: {body}"
    );
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

/// `body` decoded from `encoding`, as the browser would.
fn decoded(encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match encoding {
        Some("br") => brotli::Decompressor::new(body, 4096)
            .read_to_end(&mut out)
            .expect("brotli the browser decodes"),
        Some("gzip") => flate2::read::GzDecoder::new(body)
            .read_to_end(&mut out)
            .expect("gzip the browser decodes"),
        None => return body.to_vec(),
        Some(other) => panic!("a coding the server never sends: {other}"),
    };
    out
}

const SCRIPT: &str = "text/javascript; charset=utf-8";
const WASM: &str = "application/wasm";
const SCRIPT_MARK: &[u8] = b"export class Viewer";
const WASM_MARK: &[u8] = b"\0asm";

#[test_case(script_url, SCRIPT, SCRIPT_MARK, "gzip, deflate, br", Some("br"); "the script as brotli")]
#[test_case(script_url, SCRIPT, SCRIPT_MARK, "gzip, deflate", Some("gzip"); "the script as gzip")]
#[test_case(script_url, SCRIPT, SCRIPT_MARK, "identity", None; "the script as it is")]
#[test_case(wasm_url, WASM, WASM_MARK, "gzip, deflate, br", Some("br"); "the wasm as brotli")]
#[test_case(wasm_url, WASM, WASM_MARK, "gzip, deflate", Some("gzip"); "the wasm as gzip")]
#[test_case(wasm_url, WASM, WASM_MARK, "identity", None; "the wasm as it is")]
#[tokio::test]
async fn an_asset_is_served_hashed_immutable_and_in_the_coding_the_request_takes(
    url: fn() -> String,
    content_type: &str,
    marker: &[u8],
    accept: &str,
    encoding: Option<&str>,
) {
    let (addr, _dir) = start().await;
    let path = url();
    if !embedded() {
        let response = http::request(addr, "GET", &path, &[], "").await;
        assert_eq!(response.status, 503);
        assert_eq!(response.body, NOT_BUILT);
        assert_eq!(
            response.header("cache-control"),
            Some(Policy::Personal.value()),
            "a 503 is never cached"
        );
        return;
    }
    let identity = http::send_raw(
        addr,
        "GET",
        &path,
        &[("Accept-Encoding", "identity")],
        None,
        b"",
    )
    .await;
    assert_eq!(identity.status, 200);
    assert!(
        identity
            .body
            .windows(marker.len())
            .any(|window| window == marker),
        "{path}"
    );
    let response = http::send_raw(
        addr,
        "GET",
        &path,
        &[("Accept-Encoding", accept)],
        None,
        b"",
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some(content_type));
    assert_eq!(response.header("content-encoding"), encoding, "{path}");
    assert_eq!(response.header("vary"), Some("Accept-Encoding"));
    assert_eq!(
        response.header("cache-control"),
        Some(Policy::Immutable.value()),
        "a hashed name is immutable for a year"
    );
    assert_eq!(
        decoded(encoding, &response.body),
        identity.body,
        "{path} decodes to the plain bytes"
    );
    if encoding.is_some() {
        assert!(
            response.body.len() < identity.body.len() / 2,
            "{path}: the prebuilt variant is worth sending"
        );
    }
}

#[tokio::test]
async fn the_plain_names_are_gone_with_the_hash() {
    let (addr, _dir) = start().await;
    let plain = http::request(addr, "GET", "/viewer/kiewer_viewer.js", &[], "").await;
    assert_eq!(plain.status, if embedded() { 404 } else { 503 });
}

#[tokio::test]
async fn other_viewer_paths_are_not_found() {
    let (addr, _dir) = start().await;
    let (status, _) = http::get(addr, "/viewer/other.wasm").await;
    assert_eq!(status, 404);
}
