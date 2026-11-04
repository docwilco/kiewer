//! The schematic page through a real listener: the hand-off sheet at
//! `/schematic`, its file at a fixed URL, and a design's sheets at
//! `/{handle}/{slug}/schematic` with the sheet tree in the panel. The
//! sheet tree fixture of `kicad_core` uploaded as a design with no board
//! shows the tree, and its design page (the base, branch and revision
//! forms) sends the browser to the schematic page. The Postgres tests
//! skip without `DATABASE_URL`, like `tests/it/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::design;
use kiewer_lib::session;
use kiewer_lib::web::AppState;
use kiewer_lib::web::schematic::SCHEMATIC_PATH;
use kiewer_lib::web::viewer::{NOT_BUILT, embedded, page_script_url, script_url};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

const SCHEMATIC_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../viewer-drop/board/midi8knob.kicad_sch"
);

/// The sheet tree fixture: a root, a child beside it, a child in a
/// subfolder that the first child reuses under a third sheet symbol.
const ROOT: &[u8] = include_bytes!("../../../kicad_core/tests/fixtures/sheet_tree/hier.kicad_sch");
const POWER: &[u8] =
    include_bytes!("../../../kicad_core/tests/fixtures/sheet_tree/power.kicad_sch");
const IO: &[u8] = include_bytes!("../../../kicad_core/tests/fixtures/sheet_tree/sub/io.kicad_sch");

fn state(db: PgPool, dir: &TempDir) -> AppState {
    http::state(db, Arc::new(FsBlobStore::new(dir.path())))
}

async fn start() -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let db = kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool");
    let state = state(db, &dir);
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
    assert!(
        body.contains(&format!("from \"{}\"", page_script_url())),
        "the gestures and the beacon are the page script's, as on the board page: {body}"
    );
    assert!(
        body.contains(&format!(
            r#"<link rel="modulepreload" href="{}">"#,
            script_url()
        )),
        "{body}"
    );
    assert!(body.contains("load_schematic"), "{body}");
    assert!(body.contains("#f5f4ef"), "KiCad's paper colour: {body}");
    assert_eq!(
        body.contains(NOT_BUILT.trim_end()),
        !embedded(),
        "the page says when the viewer is not built in"
    );
    // One sheet: a root row with the fixed URL, the panel hidden for
    // want of a tree, the net block and the sheet-entering click.
    assert!(
        body.contains(&format!(
            r#"data-fragment="" data-path="/" data-url="{SCHEMATIC_PATH}""#
        )),
        "the root row: {body}"
    );
    assert!(
        body.contains(r#"<nav id="sheets" aria-label="sheets" hidden>"#),
        "{body}"
    );
    for id in ["net-tip", "net-pinned", "net-search", "net-query"] {
        assert!(body.contains(&format!(r#"id="{id}""#)), "{id}: {body}");
    }
    assert!(body.contains("sheet_symbol_at"), "{body}");
    assert!(body.contains("kiewer-sheet"), "{body}");
    assert!(
        body.contains("hashchange"),
        "the fragment restores the sheet: {body}"
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

#[test_case("/Not%20A%20Handle/x/schematic"; "the base page")]
#[test_case("/Not%20A%20Handle/x/b/main/schematic"; "the branch page")]
#[test_case("/Not%20A%20Handle/x/r/0000/schematic"; "the revision page")]
#[test_case("/a/b/b/Not%20A%20Branch/schematic"; "a branch that is no branch name")]
#[test_case("/a/b/r/nothex/schematic"; "a revision that is no hash")]
#[tokio::test]
async fn a_path_that_names_no_handle_slug_branch_or_hash_is_not_found_before_any_lookup(
    path: &str,
) {
    // The pool reaches nothing: a 404 here proves the name check comes
    // first, as on the design page.
    let (addr, _dir) = start().await;
    let (status, body) = http::get(addr, path).await;
    assert_eq!(status, 404, "{path}: {body}");
    assert!(body.contains("no such"), "{path}: {body}");
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "uploader".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("u").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    let id = session::start(db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, signed_in.account_id, handle)
}

/// The sheet panel of the fixture's page: four rows, the root first,
/// each subtree before the next sibling, io's file under two paths, and
/// the way back to the design page.
fn assert_sheet_panel(body: &str, design_path: &str) {
    assert!(
        body.contains("<title>hier schematic - kiewer</title>"),
        "{body}"
    );
    assert!(
        body.contains(r#"<nav id="sheets" aria-label="sheets">"#),
        "shown: {body}"
    );
    let root_url = format!("/blob/{}", BlobHash::of(ROOT));
    let power_url = format!("/blob/{}", BlobHash::of(POWER));
    let io_url = format!("/blob/{}", BlobHash::of(IO));
    let rows = [
        (0, "", "", "", "/", &root_url, "hier"),
        (1, "0", "0", "sheet=power", "/power", &power_url, "power"),
        (2, "1", "0", "sheet=power/reg", "/power/reg", &io_url, "reg"),
        (3, "0", "1", "sheet=io", "/io", &io_url, "io"),
    ];
    let mut last = 0;
    for (index, parent, symbol, fragment, path, url, name) in rows {
        let row = format!(
            r#"<li data-index="{index}" data-parent="{parent}" data-symbol="{symbol}" data-fragment="{fragment}" data-path="{path}" data-url="{url}""#
        );
        let at = body
            .find(&row)
            .unwrap_or_else(|| panic!("row {index}: {body}"));
        assert!(at >= last, "row {index} out of order: {body}");
        last = at;
        assert!(
            body[at..].contains(&format!(r##"<a href="#{fragment}">{name}</a>"##)),
            "row {index}'s link: {body}"
        );
    }
    assert_eq!(
        body.matches(&io_url).count(),
        2,
        "one file, two rows: {body}"
    );
    assert!(
        body.contains(&format!(
            r#"<a href="{design_path}?design">design page</a>"#
        )),
        "back to the design page, asked to stay: {body}"
    );
}

#[tokio::test]
async fn a_schematic_only_design_lists_its_sheet_tree_and_opens_on_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let addr = http::serve_on_free_port(state(db.clone(), &dir)).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", ""),
                Part::text("name", ""),
                Part::text("message", "the sheet tree fixture"),
                Part::file("hier.kicad_sch", ROOT),
                Part::file("power.kicad_sch", POWER),
                Part::file("sub/io.kicad_sch", IO),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let design_path = format!("/{handle}/hier");
    assert_eq!(posted.header("location"), Some(design_path.as_str()));
    let schematic_path = format!("{design_path}/schematic");

    // The design page, with no board, opens on the schematic page.
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 303, "{}", page.body);
    assert_eq!(page.header("location"), Some(schematic_path.as_str()));
    let branch = http::request(addr, "GET", &format!("{design_path}/b/main"), &[], "").await;
    assert_eq!(branch.status, 303, "{}", branch.body);
    assert_eq!(
        branch.header("location"),
        Some(schematic_path.as_str()),
        "the default branch's page is the bare schematic page"
    );
    let shown = design::show(&db, handle.as_str(), "hier")
        .await
        .expect("reads")
        .expect("the design");
    let hash = shown.revision.hash;
    let revision = http::request(addr, "GET", &format!("{design_path}/r/{hash}"), &[], "").await;
    assert_eq!(revision.status, 303, "{}", revision.body);
    let revision_schematic = format!("{design_path}/r/{hash}/schematic");
    assert_eq!(
        revision.header("location"),
        Some(revision_schematic.as_str())
    );

    let page = http::request(addr, "GET", &schematic_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_sheet_panel(&page.body, &design_path);

    // The revision form of the page shows the same tree, with the
    // revision's own design page behind it.
    let page = http::request(addr, "GET", &revision_schematic, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("sheet=power/reg"), "{}", page.body);
    assert!(
        page.body.contains(&format!(
            r#"<a href="{design_path}/r/{hash}?design">design page</a>"#
        )),
        "{}",
        page.body
    );
    // The design page itself, asked to stay, still shows the revision
    // with its files and links, for the settings and the upload.
    let stayed = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert_eq!(stayed.status, 200, "{}", stayed.body);
    assert!(
        stayed.body.contains("no board in this revision"),
        "{}",
        stayed.body
    );
    assert!(
        stayed.body.contains("the sheet tree fixture"),
        "{}",
        stayed.body
    );
    let stayed = http::request(
        addr,
        "GET",
        &format!("{design_path}/r/{hash}?design"),
        &[],
        "",
    )
    .await;
    assert_eq!(stayed.status, 200, "{}", stayed.body);
    let (status, _) = http::get(addr, &format!("{design_path}/b/nope/schematic")).await;
    assert_eq!(status, 404, "a branch the design lacks");
    let (status, _) = http::get(addr, &format!("{design_path}/schematic/extra")).await;
    assert_eq!(status, 404);
}
