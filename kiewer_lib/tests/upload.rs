//! Uploading a revision and viewing it, over HTTP (#16): the upload form
//! and its POST for a signed-in person, the design page with the viewer
//! at the head of the default branch, the blob route, and the caps
//! through `kiewer_lib::upload` directly; and (#17) a zip and a dropped
//! folder with their paths, the skip list, the rooting with its warning,
//! the zip caps against the central directory, and the hourly allowance.
//! The Postgres tests skip without `DATABASE_URL`, like `tests/db.rs`;
//! the route-priority test and the cap tests need no database.

use std::io::{Cursor, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::Policy;
use kiewer_lib::error::Refusal;
use kiewer_lib::session;
use kiewer_lib::upload::{self, Author, Incoming, Limits, Outcome, Target};
use kiewer_lib::{Error, Result};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const BOARD: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
const PROJECT: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");
/// A sheet below the root sheet: the least the parser takes.
const LOWER_SHEET: &[u8] = b"(kicad_sch (version 20250114) (generator \"test\"))";

/// A zip as Explorer or `zip -r` writes one: a directory entry for each
/// of `folders`, then `entries` (path, bytes) deflated.
fn zip_of(folders: &[&str], entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for folder in folders {
        writer.add_directory(*folder, options).expect("a folder");
    }
    for (path, bytes) in entries {
        writer.start_file(*path, options).expect("an entry");
        writer.write_all(bytes).expect("its bytes");
    }
    writer.finish().expect("a zip").into_inner()
}

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
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
    for expected in [
        r#"accept=".kicad_pcb,.kicad_sch,.kicad_pro,.zip""#,
        r#"<input type="file" name="files" multiple webkitdirectory>"#,
        "webkitRelativePath",
        "webkitGetAsEntry",
    ] {
        assert!(
            form.body.contains(expected),
            "the folder picker and the drop send paths: {expected}: {}",
            form.body
        );
    }
}

/// What the design page says after the first upload of the hand-off files.
fn assert_design_page(body: &str, handle: &Handle) {
    let board_hash = BlobHash::of(BOARD);
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
        body.contains(r#"/f/midi8knob.kicad_sch" download="midi8knob.kicad_sch""#),
        "every file links its download under the revision (#24): {body}"
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
    // A revision with no board opens on its schematic (tests/schematic.rs
    // has the page itself).
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 303, "{}", page.body);
    let schematic_path = format!("{design_path}/schematic");
    assert_eq!(page.header("location"), Some(schematic_path.as_str()));
    let page = http::request(addr, "GET", &schematic_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body
            .contains(&format!("/blob/{}", BlobHash::of(SCHEMATIC))),
        "the sheet is the new revision's: {}",
        page.body
    );
    // The design page stays when asked, with the revision's details.
    let page = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
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
                Part::file("x.kicad_prl", b"{}"),
            ],
        )
        .await;
    assert_eq!(nothing.status, 422, "{}", nothing.body);
    assert!(nothing.body.contains("no KiCad file"), "{}", nothing.body);
    assert_eq!(nothing.header("cache-control"), Some("private, no-store"));

    let not_a_zip = browser
        .post_multipart("/upload", &[Part::file("x.zip", b"PK")])
        .await;
    assert_eq!(not_a_zip.status, 422, "{}", not_a_zip.body);
    assert!(
        not_a_zip.body.contains("x.zip:"),
        "a zip that is not one is refused naming it: {}",
        not_a_zip.body
    );

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
async fn a_blob_nobody_stored_is_not_found_and_a_blocked_one_is_gone() {
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
    assert_eq!(status, 410, "blocked: not served");
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

/// The paths the design page lists, in order: what the tree holds.
fn listed_paths(page: &str) -> Vec<&str> {
    page.match_indices(r#" download=""#)
        .map(|(at, _)| {
            let rest = &page[at..];
            let start = rest.find('>').expect("the link's text") + 1;
            let end = rest.find("</a>").expect("the link's end");
            &rest[start..end]
        })
        .collect()
}

#[tokio::test]
async fn an_explorer_zip_of_the_hand_off_project_uploads_rooted_with_the_sheet_paths_kept() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    // Explorer's "Compressed (zipped) folder" of the project folder: the
    // folder itself wraps everything, with KiCad's backups, lock file,
    // autosave and save file, a dot-directory, Finder's folder, and the
    // files KiCad keeps beside the project. Every skipped file is garbage
    // where a KiCad file would be refused, so keeping one would fail.
    let zip = zip_of(
        &[
            "midi8knob/",
            "midi8knob/sub/",
            "midi8knob/midi8knob-backups/",
            "midi8knob/.git/",
            "__MACOSX/",
            "__MACOSX/midi8knob/",
        ],
        &[
            ("midi8knob/midi8knob.kicad_pro", PROJECT),
            ("midi8knob/midi8knob.kicad_pcb", BOARD),
            ("midi8knob/midi8knob.kicad_sch", SCHEMATIC),
            ("midi8knob/sub/power.kicad_sch", LOWER_SHEET),
            (
                "midi8knob/midi8knob-backups/midi8knob-2026-11-02_120000.zip",
                b"PK\x03\x04 not a zip",
            ),
            (
                "midi8knob/midi8knob-backups/midi8knob.kicad_pcb",
                b"(kicad_pcb garbage",
            ),
            ("midi8knob/~midi8knob.kicad_pcb.lck", b"{}"),
            ("midi8knob/_autosave-midi8knob.kicad_sch", b"(garbage"),
            ("midi8knob/midi8knob-save.kicad_pcb", b"(garbage"),
            ("midi8knob/.git/HEAD", b"ref: refs/heads/main\n"),
            (
                "__MACOSX/midi8knob/._midi8knob.kicad_pcb",
                b"\x00\x05\x16\x07",
            ),
            ("midi8knob/fp-info-cache", b"0\n"),
            ("midi8knob/README.md", b"# not kept\n"),
        ],
    );
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("message", "from a zip"),
                Part::file("midi8knob.zip", &zip),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let design_path = format!("/{handle}/midi8knob");
    assert_eq!(posted.header("location"), Some(design_path.as_str()));
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        listed_paths(&page.body),
        [
            "midi8knob.kicad_pcb",
            "midi8knob.kicad_pro",
            "midi8knob.kicad_sch",
            "sub/power.kicad_sch"
        ],
        "the wrapper folder is gone, the sheet's path stays, nothing skipped is in: {}",
        page.body
    );
    assert!(
        page.body.contains("<dt>Footprints</dt><dd>78</dd>"),
        "the board came out of the zip whole: {}",
        page.body
    );
}

#[tokio::test]
async fn a_dropped_folder_sends_paths_and_two_projects_yield_the_shallowest_with_a_warning() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    // A dropped folder `designs` holding two projects, as the page's
    // script sends it: one part per file, named by its path. The backups
    // folder comes along as it does on disk, with the zip KiCad writes
    // there at every save, and so does a zip beside the project, as its
    // Archive Project command leaves one. Neither is expanded: their
    // bytes are no zip, so looking at one would refuse the upload, and a
    // real one would put an old snapshot at the top of the tree.
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("message", "two projects"),
                Part::file("designs/midi8knob/midi8knob.kicad_pro", PROJECT),
                Part::file("designs/midi8knob/midi8knob.kicad_pcb", BOARD),
                Part::file("designs/midi8knob/midi8knob.kicad_sch", SCHEMATIC),
                Part::file("designs/midi8knob/sub/power.kicad_sch", LOWER_SHEET),
                Part::file(
                    "designs/midi8knob/midi8knob-backups/midi8knob.kicad_sch",
                    b"(garbage",
                ),
                Part::file(
                    "designs/midi8knob/midi8knob-backups/midi8knob-2026-11-02_120000.zip",
                    b"PK not a zip",
                ),
                Part::file("designs/midi8knob/midi8knob-archive.zip", b"PK not a zip"),
                Part::file("designs/other/deep/other.kicad_pro", PROJECT),
                Part::file("designs/other/deep/other.kicad_sch", LOWER_SHEET),
            ],
        )
        .await;
    assert_eq!(
        posted.status, 200,
        "pushed, and back on the form with the warning: {}",
        posted.body
    );
    let design_path = format!("/{handle}/midi8knob");
    for expected in [
        format!(r#"<a href="{design_path}">{design_path}</a>"#),
        "the upload holds more than one project: designs/midi8knob/midi8knob.kicad_pro was taken as the project; the others are designs/other/deep/other.kicad_pro".to_owned(),
        "left out, outside the project folder designs/midi8knob: designs/other/deep/other.kicad_pro, designs/other/deep/other.kicad_sch".to_owned(),
    ] {
        assert!(posted.body.contains(&expected), "{expected}: {}", posted.body);
    }
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        listed_paths(&page.body),
        [
            "midi8knob.kicad_pcb",
            "midi8knob.kicad_pro",
            "midi8knob.kicad_sch",
            "sub/power.kicad_sch"
        ],
        "the shallowest project, rooted, without the backups: {}",
        page.body
    );
}

/// A zip written to `incoming` as the handler writes the part: begun as
/// an archive, written in chunks, ended (expanded).
async fn receive_zip(incoming: &mut Incoming, name: &str, bytes: &[u8]) -> Result<()> {
    incoming.begin_archive(name).await?;
    for piece in bytes.chunks(4096) {
        incoming.write(piece).await?;
    }
    incoming.end().await
}

/// `bytes` written over the first central directory entry of `zip`, `at`
/// bytes past its signature.
fn patch_directory(zip: &mut [u8], at: usize, bytes: &[u8]) {
    let entry = zip
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .expect("a central directory entry");
    zip[entry + at..entry + at + bytes.len()].copy_from_slice(bytes);
}

/// The declared uncompressed size of the first entry rewritten: what a
/// zip bomb's directory does. An entry made to lie this way is refused
/// the moment it is inflated, so a test that gets any other outcome knows
/// it was not.
fn declare_size(zip: &mut [u8], size: u32) {
    patch_directory(zip, 24, &size.to_le_bytes());
}

/// The sheet-shaped bytes of `size` bytes, at least 11. Never parsed: the
/// tests over `Incoming` stop before `push`.
fn sized_sheet(size: usize) -> Vec<u8> {
    format!("(kicad_sch{})", " ".repeat(size - 11)).into_bytes()
}

#[tokio::test]
async fn a_zips_caps_are_checked_against_its_directory_before_anything_is_inflated() {
    let caps = Limits {
        file_bytes: 1000,
        revision_bytes: 1500,
        zip_entries: 4,
        ..Limits::DEFAULT
    };

    // Sheets the directory says sum to more than the revision cap, after
    // a first entry that lies: inflated first, it would refuse otherwise.
    let mut over = zip_of(
        &[],
        &[
            ("a.kicad_sch", &sized_sheet(200)),
            ("b.kicad_sch", &sized_sheet(900)),
            ("c.kicad_sch", &sized_sheet(900)),
        ],
    );
    declare_size(&mut over, 1);
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "over.zip", &over)
        .await
        .expect_err("over the revision cap");
    assert!(
        matches!(
            err,
            Error::Refused(Refusal::RevisionTooLarge { limit_mb: 0 })
        ),
        "{err}"
    );

    // One sheet the directory says is over the file cap, named, after the
    // same lying entry.
    let mut big = zip_of(
        &[],
        &[
            ("a.kicad_sch", &sized_sheet(200)),
            ("sub/big.kicad_sch", &sized_sheet(1001)),
        ],
    );
    declare_size(&mut big, 1);
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "big.zip", &big)
        .await
        .expect_err("over the file cap");
    assert!(
        matches!(err, Error::Refused(Refusal::FileTooLarge { ref path, limit_mb: 0 }) if path == "sub/big.kicad_sch"),
        "{err}"
    );

    // More entries than the server reads, folders counted.
    let many = zip_of(
        &["a/", "b/"],
        &[
            ("a/a.kicad_sch", &sized_sheet(20)),
            ("b/b.kicad_sch", &sized_sheet(20)),
            ("README.md", b"#"),
        ],
    );
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "many.zip", &many)
        .await
        .expect_err("too many entries");
    assert!(
        matches!(err, Error::Refused(Refusal::TooManyEntries { ref path, count: 5, limit: 4 }) if path == "many.zip"),
        "{err}"
    );

    // The count comes before the zip reader loads the directory, which
    // would hold every entry in memory: these headers are no zip at all,
    // and the reader would say that instead.
    let headers = b"PK\x01\x02".repeat(5);
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "headers.zip", &headers)
        .await
        .expect_err("too many entries");
    assert!(
        matches!(
            err,
            Error::Refused(Refusal::TooManyEntries {
                count: 5,
                limit: 4,
                ..
            })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_backups_folder_in_a_zip_is_never_inflated_nor_counted() {
    let caps = Limits {
        file_bytes: 1000,
        revision_bytes: 1500,
        ..Limits::DEFAULT
    };
    // The backup is over both caps, and its directory entry lies, so
    // counting it or inflating it would each refuse the upload.
    let mut backups = zip_of(
        &[],
        &[
            ("demo/demo-backups/demo.kicad_sch", &sized_sheet(5000)),
            ("demo/demo.kicad_sch", &sized_sheet(20)),
        ],
    );
    declare_size(&mut backups, 4000);
    let mut incoming = Incoming::new(caps).unwrap();
    receive_zip(&mut incoming, "backups.zip", &backups)
        .await
        .expect("the backups folder is not looked at");
    assert_eq!((incoming.len(), incoming.total()), (1, 20));
}

#[tokio::test]
async fn a_zip_written_with_backslashes_keeps_its_folders() {
    // What Windows PowerShell's Compress-Archive writes.
    let zip = zip_of(
        &[],
        &[
            ("demo\\demo.kicad_sch", &sized_sheet(20)),
            ("demo\\demo-backups\\old.kicad_sch", &sized_sheet(30)),
        ],
    );
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    receive_zip(&mut incoming, "demo.zip", &zip)
        .await
        .expect("a zip");
    assert_eq!(
        (incoming.len(), incoming.total()),
        (1, 20),
        "the sheet is kept and the backup is skipped by its folder"
    );
}

/// A sheet beside `backups` zips stored whole, as `zip -r` keeps them: each
/// inner end-of-directory record stays readable in the outer's tail.
fn zip_holding_stored_zips(backups: usize) -> Vec<u8> {
    let inner = zip_of(&[], &[("old.kicad_sch", &sized_sheet(30))]);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer
        .start_file("demo/demo.kicad_sch", deflated)
        .expect("an entry");
    writer.write_all(&sized_sheet(20)).expect("its bytes");
    for backup in 0..backups {
        writer
            .start_file(format!("demo/demo-backups/old-{backup}.zip"), stored)
            .expect("an entry");
        writer.write_all(&inner).expect("its bytes");
    }
    writer.finish().expect("a zip").into_inner()
}

#[tokio::test]
async fn a_zip_with_too_many_end_records_is_refused_before_it_is_opened() {
    // A valid zip with its end record repeated. The zip reader tries each
    // record in turn and walks the directory again for it, so these are
    // what multiplies the cost of opening a hostile zip; this one, whose
    // last record is good, would open.
    let mut zip = zip_of(&[], &[("a.kicad_sch", &sized_sheet(20))]);
    let record = zip[zip.len() - 22..].to_vec();
    zip.extend(record.repeat(32));
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    let err = receive_zip(&mut incoming, "ends.zip", &zip)
        .await
        .expect_err("thirty-three records");
    assert!(
        matches!(err, Error::Refused(Refusal::BadArchive { ref path, ref reason })
            if path == "ends.zip" && reason.contains("33 end-of-directory records")),
        "{err}"
    );

    // At the limit the same zip opens.
    zip.truncate(zip.len() - 22);
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    receive_zip(&mut incoming, "ends.zip", &zip)
        .await
        .expect("thirty-two records");
    assert_eq!(incoming.len(), 1);
}

#[tokio::test]
async fn a_small_project_with_stored_backups_is_read_as_one_zip() {
    // Twenty-five backups small enough that every record is in the tail.
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    receive_zip(&mut incoming, "demo.zip", &zip_holding_stored_zips(25))
        .await
        .expect("the inner records are not mistaken for an attack");
    assert_eq!((incoming.len(), incoming.total()), (1, 20));
}

#[tokio::test]
async fn a_zip_streamed_with_data_descriptors_is_read_from_its_directory() {
    // What a streaming writer such as macOS's Archive Utility produces:
    // the local headers carry no sizes, the central directory does.
    let mut writer = zip::ZipWriter::new_stream(Vec::new());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer
        .start_file("demo/demo.kicad_sch", options)
        .expect("an entry");
    writer.write_all(&sized_sheet(20)).expect("its bytes");
    let zip = writer.finish().expect("a zip").into_inner();
    assert_ne!(
        zip[6] & 0b1000,
        0,
        "the local header defers to a descriptor"
    );
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    receive_zip(&mut incoming, "demo.zip", &zip)
        .await
        .expect("a zip");
    assert_eq!((incoming.len(), incoming.total()), (1, 20));
}

// Offsets from the central header's signature: the general purpose flags
// at 8 (bit 0 is encrypted), the compression method at 10 (12 is bzip2).
#[test_case(8, &[1], "encrypted"; "an encrypted entry")]
#[test_case(10, &[12, 0], "deflate"; "an entry in bzip2")]
#[tokio::test]
async fn a_zip_the_server_will_not_inflate_is_refused_from_its_directory(
    at: usize,
    patch: &[u8],
    says: &str,
) {
    let mut zip = zip_of(&[], &[("a.kicad_sch", &sized_sheet(200))]);
    patch_directory(&mut zip, at, patch);
    let mut incoming = Incoming::new(Limits::DEFAULT).unwrap();
    let err = receive_zip(&mut incoming, "demo.zip", &zip)
        .await
        .expect_err("refused");
    assert!(
        matches!(err, Error::Refused(Refusal::BadArchive { ref path, ref reason })
            if path == "demo.zip" && reason.starts_with("a.kicad_sch ") && reason.contains(says)),
        "{err}"
    );
}

#[tokio::test]
async fn a_zip_that_lies_about_an_entry_or_is_itself_over_the_cap_is_refused() {
    let caps = Limits {
        file_bytes: 1000,
        revision_bytes: 1500,
        ..Limits::DEFAULT
    };

    // A directory that lies about an entry's size, either way: a bomb's
    // small number, and a number larger than the entry.
    let mut bomb = zip_of(&[], &[("a.kicad_sch", &sized_sheet(500))]);
    declare_size(&mut bomb, 1);
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "bomb.zip", &bomb)
        .await
        .expect_err("inflates past its declared size");
    assert!(
        matches!(err, Error::Refused(Refusal::BadArchive { ref path, ref reason }) if path == "bomb.zip" && reason.starts_with("a.kicad_sch: ") && reason.contains("declared")),
        "{err}"
    );
    let mut short = zip_of(&[], &[("a.kicad_sch", &sized_sheet(500))]);
    declare_size(&mut short, 600);
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "short.zip", &short)
        .await
        .expect_err("holds less than declared");
    assert!(
        matches!(err, Error::Refused(Refusal::BadArchive { ref path, .. }) if path == "short.zip"),
        "{err}"
    );

    // A zip's own bytes are bounded by the revision cap.
    let mut incoming = Incoming::new(caps).unwrap();
    let err = receive_zip(&mut incoming, "huge.zip", &[0; 1501])
        .await
        .expect_err("a zip over the revision cap");
    assert!(
        matches!(
            err,
            Error::Refused(Refusal::RevisionTooLarge { limit_mb: 0 })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn the_hourly_allowance_refuses_the_push_over_it_through_the_library() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = FsBlobStore::new(dir.path());
    let (account_id, handle) = person(&db).await;
    let author = Author { account_id, handle };
    let one_an_hour = Limits {
        uploads_per_hour: 1,
        ..Limits::DEFAULT
    };
    let sheet = |n: u32| {
        format!(
            "(kicad_sch (version 20250114) (uuid \"{}-{n}\"))",
            fresh("s")
        )
    };
    let mut incoming = Incoming::new(one_an_hour).unwrap();
    receive(&mut incoming, "a.kicad_sch", sheet(1).as_bytes(), 64)
        .await
        .unwrap();
    let first = upload::push(
        &db,
        &blobs,
        &one_an_hour,
        &author,
        Target::New {
            name: Some("Paced".to_owned()),
        },
        None,
        incoming,
    )
    .await
    .expect("the first push of the hour");
    let Outcome::Pushed(first) = first else {
        panic!("a new design has no head to have moved");
    };
    assert!(first.warnings.is_empty());

    let mut incoming = Incoming::new(one_an_hour).unwrap();
    receive(&mut incoming, "a.kicad_sch", sheet(2).as_bytes(), 64)
        .await
        .unwrap();
    let second = upload::push(
        &db,
        &blobs,
        &one_an_hour,
        &author,
        Target::Existing {
            owner: author.handle.clone(),
            slug: first.design.slug.clone(),
            branch: upload::BranchTarget::Default,
        },
        None,
        incoming,
    )
    .await
    .expect_err("the second push of the hour");
    assert!(
        matches!(second, Error::Refused(Refusal::RateLimited { limit: 1, retry_after_secs }) if (1..=3600).contains(&retry_after_secs)),
        "{second}"
    );
}

#[tokio::test]
async fn the_thirty_first_upload_in_an_hour_gets_429_with_retry_after() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let sheet = |n: u32| {
        format!(
            "(kicad_sch (version 20250114) (uuid \"{}-{n}\"))",
            fresh("s")
        )
    };
    let first = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "Paced"),
                Part::file("paced.kicad_sch", sheet(1).as_bytes()),
            ],
        )
        .await;
    assert_eq!(first.status, 303, "{}", first.body);
    let design = format!("{handle}/paced");
    for n in 2..=30 {
        let next = browser
            .post_multipart(
                "/upload",
                &[
                    Part::text("design", &design),
                    Part::file("paced.kicad_sch", sheet(n).as_bytes()),
                ],
            )
            .await;
        assert_eq!(next.status, 303, "upload {n}: {}", next.body);
    }
    let over = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", &design),
                Part::file("paced.kicad_sch", sheet(31).as_bytes()),
            ],
        )
        .await;
    assert_eq!(over.status, 429, "{}", over.body);
    let retry_after: u64 = over
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .expect("whole seconds");
    assert!((1..=3600).contains(&retry_after), "{retry_after}");
    assert!(
        over.body.contains("30 uploads in the last hour"),
        "{}",
        over.body
    );
    assert_eq!(over.header("cache-control"), Some("private, no-store"));
    let (listed,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM design_revision dr JOIN design d ON d.id = dr.design_id
         JOIN account a ON a.id = d.owner_id WHERE a.handle = $1",
    )
    .bind(handle.as_str())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(listed, 30, "the thirty-first made no revision");
}

#[tokio::test]
async fn a_refused_upload_counts_against_the_hourly_allowance() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    // Refusals are what a hostile zip gets, and each one cost the server
    // the work of refusing it: they spend the allowance as pushes do.
    for n in 1..=30 {
        let refused = browser
            .post_multipart("/upload", &[Part::file("bad.kicad_pro", b"{ not json")])
            .await;
        assert_eq!(refused.status, 422, "upload {n}: {}", refused.body);
    }
    let good = format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s"));
    let over = browser
        .post_multipart("/upload", &[Part::file("good.kicad_sch", good.as_bytes())])
        .await;
    assert_eq!(over.status, 429, "{}", over.body);
    let retry_after: u64 = over
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .expect("whole seconds");
    assert!((1..=3600).contains(&retry_after), "{retry_after}");
    let (recorded,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM upload_refusal r JOIN account a ON a.id = r.account_id
         WHERE a.handle = $1",
    )
    .bind(handle.as_str())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        recorded, 30,
        "the 429 is not recorded, or being limited would extend the limit"
    );
}

#[tokio::test]
async fn static_routes_keep_priority_over_the_design_page() {
    let dir = scratch_dir!();
    let state = http::state(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
    );
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
