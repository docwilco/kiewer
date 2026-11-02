//! Uploading a revision and viewing it, over HTTP (#16): the upload form
//! and its POST for a signed-in person, the design page with the viewer
//! at the head of the default branch, the blob route, and the caps
//! through `kiewer_lib::upload` directly. The Postgres tests skip without
//! `DATABASE_URL`, like `tests/db.rs`; the route-priority test needs no
//! database.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Auth, Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::error::Refusal;
use kiewer_lib::session;
use kiewer_lib::upload::{self, Author, Incoming, Limits, Target};
use kiewer_lib::web::AppState;
use kiewer_lib::web::blob::IMMUTABLE;
use kiewer_lib::{Error, Result};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;

const BOARD: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
const PROJECT: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = AppState {
        db,
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle: the uploader.
async fn person(db: &PgPool) -> (AccountId, Handle) {
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
    (signed_in.account_id, handle)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let (account_id, handle) = person(db).await;
    let id = session::start(db, account_id).await.expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, account_id, handle)
}

#[tokio::test]
async fn the_upload_form_is_for_a_signed_in_person_with_a_handle() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let mut stranger = http::Browser::new(addr);
    let cold = stranger.get("/upload").await;
    assert_eq!(cold.status, 303, "{}", cold.body);
    assert_eq!(cold.header("location"), Some("/"));
    let posted = stranger
        .post_multipart("/upload", &[Part::file("x.kicad_pcb", b"(kicad_pcb)")])
        .await;
    assert_eq!(posted.status, 303, "a stranger's upload goes nowhere");

    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let form = browser.get("/upload").await;
    assert_eq!(form.status, 200, "{}", form.body);
    assert_eq!(form.header("cache-control"), Some("private, no-store"));
    assert!(
        form.body.contains(r#"enctype="multipart/form-data""#),
        "{}",
        form.body
    );
    assert!(form.body.contains("A new design"), "{}", form.body);
    assert!(form.body.contains(handle.as_str()), "{}", form.body);
    assert!(
        form.body.contains("50 MB each and 200 MB together"),
        "the caps are said: {}",
        form.body
    );
}

/// What the design page says after the first upload of the hand-off files.
fn assert_design_page(body: &str, handle: &Handle) {
    let board_hash = BlobHash::of(BOARD);
    let schematic_hash = BlobHash::of(SCHEMATIC);
    assert!(body.contains("<canvas"), "the viewer: {body}");
    assert!(
        body.contains(&format!(r#"fetch("/blob/{board_hash}")"#)),
        "the viewer fetches the board's blob: {body}"
    );
    assert!(
        body.contains("<title>midi8knob - kiewer</title>"),
        "named after the project: {body}"
    );
    assert!(body.contains("first upload"), "the message: {body}");
    assert!(body.contains(handle.as_str()), "the author: {body}");
    for (label, value) in [
        ("Format version", "20260206"),
        ("Layers", "26 (4 copper)"),
        ("Fonts", "Consolas, Newstroke"),
        ("Nets", "75"),
        ("Footprints", "78"),
    ] {
        assert!(
            body.contains(&format!("<dt>{label}</dt><dd>{value}</dd>")),
            "{label} {value}: {body}"
        );
    }
    assert!(body.contains("<dt>Board size</dt><dd>"), "{body}");
    assert!(
        body.contains(&format!(r#"href="/blob/{schematic_hash}""#)),
        "every file links its blob: {body}"
    );
    assert!(body.contains("midi8knob.kicad_pro"), "{body}");
    assert!(!body.contains("README"), "the readme was dropped: {body}");
}

/// The board's blob: gzip as stored for a client that takes it, decoded
/// for one that does not, immutable either way.
async fn assert_blob_route(addr: SocketAddr, board_hash: &BlobHash) {
    let blob_path = format!("/blob/{board_hash}");
    let gzipped = http::request(
        addr,
        "GET",
        &blob_path,
        &[("Accept-Encoding", "gzip, deflate, br")],
        "",
    )
    .await;
    assert_eq!(gzipped.status, 200);
    assert_eq!(gzipped.header("content-encoding"), Some("gzip"));
    assert_eq!(gzipped.header("cache-control"), Some(IMMUTABLE));
    assert_eq!(gzipped.header("vary"), Some("Accept-Encoding"));
    assert_eq!(
        gzipped.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(gzipped.header("x-content-type-options"), Some("nosniff"));
    let raw = http::send(
        addr,
        "GET",
        &blob_path,
        &[("Accept-Encoding", "gzip")],
        None,
        b"",
    )
    .await;
    assert!(raw.body.len() < BOARD.len(), "compressed on the wire");
    let plain = http::request(addr, "GET", &blob_path, &[], "").await;
    assert_eq!(plain.status, 200);
    assert_eq!(plain.header("content-encoding"), None);
    assert_eq!(plain.body.as_bytes(), BOARD);
}

#[tokio::test]
async fn uploading_the_hand_off_files_makes_a_design_shown_with_the_viewer() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;

    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", ""),
                Part::text("name", ""),
                Part::text("message", "first upload"),
                Part::file("midi8knob.kicad_pcb", BOARD),
                Part::file("midi8knob.kicad_sch", SCHEMATIC),
                Part::file("midi8knob.kicad_pro", PROJECT),
                Part::file("README.md", b"# not kept\n"),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let design_path = format!("/{handle}/midi8knob");
    assert_eq!(
        posted.header("location"),
        Some(design_path.as_str()),
        "the slug comes from the project file's name"
    );

    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_design_page(&page.body, &handle);
    assert_blob_route(addr, &BlobHash::of(BOARD)).await;

    // A second upload onto the design: a new head with the first as parent.
    let form = browser.get("/upload").await;
    assert!(
        form.body
            .contains(&format!(r#"<option value="{handle}/midi8knob">"#)),
        "the design is offered: {}",
        form.body
    );
    let again = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", &format!("{handle}/midi8knob")),
                Part::text("message", "schematic only"),
                Part::file("midi8knob.kicad_sch", SCHEMATIC),
            ],
        )
        .await;
    assert_eq!(again.status, 303, "{}", again.body);
    assert_eq!(again.header("location"), Some(design_path.as_str()));
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert!(page.body.contains("schematic only"), "{}", page.body);
    assert!(
        page.body.contains("no board in this revision"),
        "{}",
        page.body
    );
    let (listed, parents, heads): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM design_revision dr JOIN design d ON d.id = dr.design_id
                 JOIN account a ON a.id = d.owner_id WHERE a.handle = $1),
                (SELECT count(*) FROM revision r JOIN design_revision dr ON dr.revision_hash = r.hash
                 JOIN design d ON d.id = dr.design_id JOIN account a ON a.id = d.owner_id
                 WHERE a.handle = $1 AND r.parent_hash IS NOT NULL),
                (SELECT count(*) FROM branch b JOIN design d ON d.id = b.design_id
                 JOIN account a ON a.id = d.owner_id WHERE a.handle = $1)",
    )
    .bind(handle.as_str())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (listed, parents, heads),
        (2, 1, 1),
        "both revisions are the design's, one chain of two, on one branch"
    );
}

#[tokio::test]
async fn an_upload_without_a_kicad_file_or_with_a_malformed_one_is_refused_with_the_reason() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, _) = signed_in(&db, addr).await;
    let nothing = browser
        .post_multipart(
            "/upload",
            &[
                Part::file("README.md", b"# no\n"),
                Part::file("x.zip", b"PK"),
            ],
        )
        .await;
    assert_eq!(nothing.status, 422, "{}", nothing.body);
    assert!(nothing.body.contains("no KiCad file"), "{}", nothing.body);
    assert_eq!(nothing.header("cache-control"), Some("private, no-store"));

    let malformed = browser
        .post_multipart(
            "/upload",
            &[Part::file(
                "bad.kicad_pcb",
                b"(kicad_pcb (version 20241229) (layers (0 \"F.Cu\" signal)) (footprint",
            )],
        )
        .await;
    assert_eq!(malformed.status, 422, "{}", malformed.body);
    assert!(
        malformed.body.contains("bad.kicad_pcb") && malformed.body.contains("line"),
        "the parser's error names the file and the line: {}",
        malformed.body
    );

    let schematic = browser
        .post_multipart(
            "/upload",
            &[Part::file(
                "bad.kicad_sch",
                b"(kicad_pcb (version 20241229))",
            )],
        )
        .await;
    assert_eq!(schematic.status, 422, "{}", schematic.body);
    assert!(
        schematic.body.contains("bad.kicad_sch") && schematic.body.contains("kicad_sch"),
        "a board where a schematic was named is refused naming the file: {}",
        schematic.body
    );
    let project = browser
        .post_multipart("/upload", &[Part::file("bad.kicad_pro", b"{ not json")])
        .await;
    assert_eq!(project.status, 422, "{}", project.body);
    assert!(project.body.contains("bad.kicad_pro"), "{}", project.body);

    let old = browser
        .post_multipart(
            "/upload",
            &[Part::file(
                "old.kicad_pcb",
                b"(kicad_pcb (version 20211014) (layers (0 \"F.Cu\" signal)))",
            )],
        )
        .await;
    assert_eq!(old.status, 422, "{}", old.body);
    assert!(
        old.body.contains("older than KiCad 9"),
        "the version is named: {}",
        old.body
    );

    let missing = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", "nothing-here"),
                Part::file("a.kicad_sch", b"(kicad_sch (version 20250114))"),
            ],
        )
        .await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert!(missing.body.contains("nothing-here"), "{}", missing.body);
}

#[tokio::test]
async fn a_blob_nobody_stored_is_not_found_and_a_blocked_one_is_too() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (status, _) = http::get(addr, "/blob/not-a-hash").await;
    assert_eq!(status, 404);
    let never = BlobHash::of(fresh("never").as_bytes());
    let (status, _) = http::get(addr, &format!("/blob/{never}")).await;
    assert_eq!(status, 404);

    let (mut browser, _, _) = signed_in(&db, addr).await;
    let bytes = format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("sch"));
    let hash = BlobHash::of(bytes.as_bytes());
    let posted = browser
        .post_multipart(
            "/upload",
            &[Part::file("blocked.kicad_sch", bytes.as_bytes())],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let (status, _) = http::get(addr, &format!("/blob/{hash}")).await;
    assert_eq!(status, 200);
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(hash.as_bytes())
        .execute(&db)
        .await
        .unwrap();
    let (status, _) = http::get(addr, &format!("/blob/{hash}")).await;
    assert_eq!(status, 404, "blocked: not served");
    let again = browser
        .post_multipart(
            "/upload",
            &[Part::file("blocked.kicad_sch", bytes.as_bytes())],
        )
        .await;
    assert_eq!(again.status, 422, "blocked: not accepted: {}", again.body);
    assert!(again.body.contains("does not accept"), "{}", again.body);
}

/// A board written to an `Incoming` under `limits`, as the handler
/// writes a part: begun, written in chunks of `chunk` bytes, ended.
async fn receive(incoming: &mut Incoming, name: &str, bytes: &[u8], chunk: usize) -> Result<()> {
    let (path, kind) = Incoming::accepts(name).expect("a KiCad file");
    incoming.begin(path, kind).await?;
    for piece in bytes.chunks(chunk) {
        incoming.write(piece).await?;
    }
    incoming.end().await
}

#[tokio::test]
async fn the_per_file_and_per_revision_caps_refuse_at_the_byte_over() {
    let small_files = Limits {
        file_bytes: 100,
        revision_bytes: 1000,
        ..Limits::DEFAULT
    };
    let mut incoming = Incoming::new(small_files).expect("a temp dir");
    receive(&mut incoming, "a.kicad_pcb", &[b'a'; 100], 7)
        .await
        .expect("at the cap is fine");
    let over = receive(&mut incoming, "b.kicad_sch", &[b'b'; 101], 7)
        .await
        .expect_err("one over the file cap");
    assert!(
        matches!(over, Error::Refused(Refusal::FileTooLarge { ref path, limit_mb: 0 }) if path == "b.kicad_sch"),
        "{over}"
    );
    let small_revision = Limits {
        file_bytes: 100,
        revision_bytes: 150,
        ..Limits::DEFAULT
    };
    let mut incoming = Incoming::new(small_revision).expect("a temp dir");
    receive(&mut incoming, "a.kicad_pcb", &[b'a'; 100], 100)
        .await
        .unwrap();
    let over = receive(&mut incoming, "b.kicad_sch", &[b'b'; 51], 51)
        .await
        .expect_err("one over the revision cap");
    assert!(
        matches!(
            over,
            Error::Refused(Refusal::RevisionTooLarge { limit_mb: 0 })
        ),
        "{over}"
    );
    assert_eq!(incoming.total(), 100, "the refused bytes were not counted");
}

#[tokio::test]
async fn the_account_cap_and_the_parse_timeout_refuse_through_commit() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = FsBlobStore::new(dir.path());
    let (account_id, handle) = person(&db).await;
    let author = Author { account_id, handle };

    let tiny_account = Limits {
        account_bytes: 100,
        ..Limits::DEFAULT
    };
    let mut incoming = Incoming::new(tiny_account).unwrap();
    receive(
        &mut incoming,
        "big.kicad_sch",
        format!(
            "(kicad_sch (version 20250114) (uuid \"{}\") {})",
            fresh("s"),
            " ".repeat(200)
        )
        .as_bytes(),
        64,
    )
    .await
    .unwrap();
    let full = upload::push(
        &db,
        &blobs,
        &tiny_account,
        &author,
        Target::New {
            name: Some("Full".to_owned()),
        },
        None,
        incoming,
    )
    .await
    .expect_err("over the account cap");
    assert!(
        matches!(full, Error::Refused(Refusal::AccountFull { limit_gb: 0 })),
        "{full}"
    );
    assert!(
        kiewer_lib::design::by_owner(&db, account_id)
            .await
            .unwrap()
            .is_empty(),
        "the refused upload made no design"
    );

    let no_time = Limits {
        parse_timeout: Duration::ZERO,
        ..Limits::DEFAULT
    };
    let mut incoming = Incoming::new(no_time).unwrap();
    receive(&mut incoming, "midi8knob.kicad_pcb", BOARD, 65536)
        .await
        .unwrap();
    assert_eq!(incoming.len(), 1);
    let slow = upload::push(
        &db,
        &blobs,
        &no_time,
        &author,
        Target::New { name: None },
        None,
        incoming,
    )
    .await
    .expect_err("no time to parse");
    assert!(
        matches!(slow, Error::Refused(Refusal::ParseTimeout { seconds: 0 })),
        "{slow}"
    );
}

#[tokio::test]
async fn static_routes_keep_priority_over_the_design_page() {
    let dir = scratch_dir!();
    let state = AppState {
        db: kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    let addr = http::serve_on_free_port(state).await;
    let sessions = http::request(addr, "GET", "/settings/sessions", &[], "").await;
    assert_eq!(
        (sessions.status, sessions.header("location")),
        (303, Some("/")),
        "the settings page, not a design"
    );
    let (status, body) = http::get(addr, "/viewer").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<canvas"), "the hand-off viewer: {body}");
    let (status, body) = http::get(addr, "/auth/github/callback").await;
    assert_eq!(
        status, 400,
        "the callback without its state cookie, not a design's 404: {body}"
    );
    let (status, _) = http::get(addr, "/api/me").await;
    assert_eq!(status, 200);
    let (status, _) = http::get(addr, "/upload").await;
    assert_eq!(status, 303, "the form sends a stranger home");
}
