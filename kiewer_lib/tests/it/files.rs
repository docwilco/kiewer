//! Per-file downloads over HTTP (#24): `/{handle}/{slug}/r/{hash}/f/{path}`
//! serves one file of a revision under its original name with a
//! `Content-Disposition`, the stored gzip for a client that takes it,
//! and immutable cache headers under the revision hash; a path the
//! tree does not hold is 404, a revision the design does not reach is
//! 404, a blocked blob is 410. The Postgres tests skip without
//! `DATABASE_URL`, like `tests/it/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::account::{self, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::Policy;
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

const BOARD: &[u8] = include_bytes!("../../../viewer-drop/board/midi8knob.kicad_pcb");

/// The smallest board that parses. A revision without a board opens on
/// its schematic, and the design page is what these tests read the
/// revision's hash and the download links from, so every upload has one.
pub(super) const BARE_BOARD: &[u8] =
    b"(kicad_pcb (version 20241229) (generator \"test\") (layers (0 \"F.Cu\" signal)))";

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "downloader".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("d").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    let id = session::start(db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, handle)
}

/// A schematic whose text is `tag`'s alone, so each upload is a new blob.
fn sheet(tag: &str) -> Vec<u8> {
    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh(tag)).into_bytes()
}

/// `files` pushed as a new design named `name`; the design page after.
async fn upload(browser: &mut http::Browser, name: &str, files: &[(&str, &[u8])]) -> String {
    let mut parts = vec![Part::text("name", name)];
    parts.extend(files.iter().map(|(path, bytes)| Part::file(path, bytes)));
    let posted = browser.post_multipart("/upload", &parts).await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let location = posted.header("location").expect("a location").to_owned();
    let page = browser.get(&location).await;
    assert_eq!(page.status, 200, "{}", page.body);
    page.body
}

/// The hash of the revision a design page shows: the `title` of its
/// short-hash code.
fn shown_revision(page: &str) -> String {
    let start = page.find("<code title=\"").expect("the revision's code") + 13;
    page[start..start + 64].to_owned()
}

/// `GET path` with the request `headers`.
async fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> http::Response {
    http::request(addr, "GET", path, headers, "").await
}

#[tokio::test]
async fn the_hand_off_board_downloads_under_its_name_with_immutable_headers() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, handle) = signed_in(&db, addr).await;
    let page = upload(&mut browser, "midi8knob", &[("midi8knob.kicad_pcb", BOARD)]).await;
    let revision = shown_revision(&page);
    let route = format!("/{handle}/midi8knob/r/{revision}/f/midi8knob.kicad_pcb");
    assert!(
        page.contains(&format!(r#"href="{route}" download="midi8knob.kicad_pcb""#)),
        "the design page links the file's download: {page}"
    );

    let gzipped = get(addr, &route, &[("Accept-Encoding", "gzip, deflate, br")]).await;
    assert_eq!(gzipped.status, 200, "{}", gzipped.body);
    assert_eq!(gzipped.header("content-encoding"), Some("gzip"));
    assert_eq!(
        gzipped.header("content-disposition"),
        Some(r#"attachment; filename="midi8knob.kicad_pcb""#)
    );
    assert_eq!(
        gzipped.header("cache-control"),
        Some(Policy::Immutable.value())
    );
    assert_eq!(gzipped.header("vary"), Some("Accept-Encoding"));
    assert_eq!(
        gzipped.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(gzipped.header("x-content-type-options"), Some("nosniff"));
    assert!(gzipped.body.len() < BOARD.len(), "compressed on the wire");

    let plain = get(addr, &route, &[]).await;
    assert_eq!(plain.status, 200);
    assert_eq!(plain.header("content-encoding"), None);
    assert_eq!(
        plain.header("cache-control"),
        Some(Policy::Immutable.value())
    );
    assert_eq!(plain.body.as_bytes(), BOARD);
}

#[tokio::test]
async fn a_nested_path_of_a_multi_file_tree_downloads_under_its_file_name() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, handle) = signed_in(&db, addr).await;
    let root = sheet("root");
    let nested = sheet("nested");
    let odd = sheet("odd");
    let page = upload(
        &mut browser,
        "nested",
        &[
            ("nested.kicad_pcb", BARE_BOARD),
            ("nested.kicad_sch", &root),
            ("sub/dir/power supply.kicad_sch", &nested),
            ("sub/50% ü.kicad_sch", &odd),
        ],
    )
    .await;
    let revision = shown_revision(&page);
    let route = format!("/{handle}/nested/r/{revision}/f/sub/dir/power%20supply.kicad_sch");
    assert!(
        page.contains(&format!(
            r#"href="{route}" download="power supply.kicad_sch""#
        )),
        "the design page links the nested file: {page}"
    );
    let plain = get(addr, &route, &[]).await;
    assert_eq!(plain.status, 200, "{}", plain.body);
    assert_eq!(
        plain.header("content-disposition"),
        Some(r#"attachment; filename="power supply.kicad_sch""#)
    );
    assert_eq!(plain.body.as_bytes(), nested);
    let top = get(
        addr,
        &format!("/{handle}/nested/r/{revision}/f/nested.kicad_sch"),
        &[],
    )
    .await;
    assert_eq!(top.body.as_bytes(), root);

    // A percent sign and a non-ASCII letter round-trip through the URL,
    // and the name goes out as a filename star.
    let escaped = format!("/{handle}/nested/r/{revision}/f/sub/50%25%20%C3%BC.kicad_sch");
    assert!(
        page.contains(&format!(r#"href="{escaped}""#)),
        "the design page links the escaped path: {page}"
    );
    let odd_file = get(addr, &escaped, &[]).await;
    assert_eq!(odd_file.status, 200, "{}", odd_file.body);
    assert_eq!(
        odd_file.header("content-disposition"),
        Some(
            r#"attachment; filename="50_ _.kicad_sch"; filename*=UTF-8''50%25%20%C3%BC.kicad_sch"#
        )
    );
    assert_eq!(odd_file.body.as_bytes(), odd);
}

#[test_case("/{mine}/four/r/{revision}/f/nothing.kicad_sch"; "a path the tree does not hold")]
#[test_case("/{mine}/four/r/{revision}/f/sub/four.kicad_sch"; "a folder the tree does not hold")]
#[test_case("/{mine}/four/r/{revision}/f/../four.kicad_sch"; "a parent segment")]
#[test_case("/{mine}/four/r/{revision}/f/%FF.kicad_pcb"; "a path that is not UTF-8")]
#[test_case("/{mine}/four/r/{foreign}/f/four.kicad_sch"; "another design's revision")]
#[test_case("/{mine}/four/r/{zeros}/f/four.kicad_sch"; "a revision nothing holds")]
#[test_case("/{mine}/four/r/not-a-hash/f/four.kicad_sch"; "text that is not a hash")]
#[test_case("/{mine}/nothing/r/{revision}/f/four.kicad_sch"; "a design that does not exist")]
#[test_case("/{theirs}/four/r/{revision}/f/four.kicad_sch"; "this revision under another design")]
#[test_case("/%00/four/r/{revision}/f/four.kicad_sch"; "a NUL in the handle")]
#[test_case("/{mine}/%00/r/{revision}/f/four.kicad_sch"; "a NUL in the slug")]
#[tokio::test]
async fn a_download_nothing_answers_for_is_404(template: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, handle) = signed_in(&db, addr).await;
    let page = upload(
        &mut browser,
        "four",
        &[
            ("four.kicad_pcb", BARE_BOARD),
            ("four.kicad_sch", &sheet("four")),
        ],
    )
    .await;
    let revision = shown_revision(&page);
    let (mut other, other_handle) = signed_in(&db, addr).await;
    let foreign = upload(
        &mut other,
        "four",
        &[
            ("four.kicad_pcb", BARE_BOARD),
            ("four.kicad_sch", &sheet("four")),
        ],
    )
    .await;
    let foreign = shown_revision(&foreign);
    assert_ne!(revision, foreign);
    let own = get(
        addr,
        &format!("/{other_handle}/four/r/{foreign}/f/four.kicad_sch"),
        &[],
    )
    .await;
    assert_eq!(own.status, 200, "but its own design reaches it");

    let path = template
        .replace("{mine}", handle.as_str())
        .replace("{theirs}", other_handle.as_str())
        .replace("{revision}", &revision)
        .replace("{foreign}", &foreign)
        .replace("{zeros}", &"0".repeat(64));
    let answer = get(addr, &path, &[]).await;
    assert_eq!(answer.status, 404, "{path}: {}", answer.body);
    assert_eq!(answer.header("content-disposition"), None, "{path}");
    assert_eq!(
        answer.header("cache-control"),
        Some(Policy::NotFound.value()),
        "{path}"
    );
}

#[tokio::test]
async fn a_blocked_blob_is_gone() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, handle) = signed_in(&db, addr).await;
    let bytes = sheet("blocked");
    let page = upload(
        &mut browser,
        "blocked",
        &[
            ("blocked.kicad_pcb", BARE_BOARD),
            ("blocked.kicad_sch", &bytes),
        ],
    )
    .await;
    let revision = shown_revision(&page);
    let route = format!("/{handle}/blocked/r/{revision}/f/blocked.kicad_sch");
    assert_eq!(get(addr, &route, &[]).await.status, 200);
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(BlobHash::of(&bytes).as_bytes())
        .execute(&db)
        .await
        .unwrap();
    let gone = get(addr, &route, &[]).await;
    assert_eq!(gone.status, 410, "{}", gone.body);
    assert_eq!(gone.header("content-disposition"), None);
    assert_eq!(
        gone.header("cache-control"),
        Some(Policy::NotFound.value()),
        "a takedown is not cached for a year"
    );
}
