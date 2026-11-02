//! Thumbnails and Open Graph over HTTP (#25, ADR 0004): uploading the
//! hand-off board renders both sizes inside the request and the design,
//! branch, revision and history pages point at them; a render past the
//! budget leaves the revision saved with a retry queued and the pages on
//! the placeholder until the retry path renders it; a revision with only
//! a schematic draws its sheet and its schematic page carries the tags; a
//! blocked file takes its thumbnail off the route and every page. The
//! Postgres tests skip without `DATABASE_URL`, like `tests/db.rs`, and
//! the renders skip on a box with no Vulkan driver, as the viewer's
//! golden test does; on CI (`CI` set) that skip is a failure, since the
//! runner has lavapipe.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Auth, Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::blob::{BlobHash, BlobStore, PutReceipt};
use kiewer_lib::cache::{Key, NoPurge, Policy, Purge, Recorder};
use kiewer_lib::design;
use kiewer_lib::revision::RevisionHash;
use kiewer_lib::session;
use kiewer_lib::thumbnail::{Size, TaskAuth, TaskQueue, Thumbnails};
use kiewer_lib::web::AppState;
use kiewer_lib::{Error, Result};
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use kiewer_viewer::headless::Headless;
use kiewer_viewer::render::offscreen::Image;
use kiewer_viewer::view::palette;
use sqlx::PgPool;
use test_case::test_case;

const BOARD: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
const PROJECT: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");

/// The public URL the test's sign-in flow is built with, which the Open
/// Graph tags' absolute URLs start with.
const PUBLIC_URL: &str = "http://127.0.0.1:1";

/// The service account a retry's token is minted as.
const TASK_ACCOUNT: &str = "kiewer-run@p.iam.gserviceaccount.com";

/// A queue that records what was queued, in order.
#[derive(Debug, Default)]
struct Recording(Mutex<Vec<RevisionHash>>);

impl Recording {
    fn queued(&self) -> Vec<RevisionHash> {
        self.0.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl TaskQueue for Recording {
    async fn enqueue(&self, revision: RevisionHash) -> Result<()> {
        self.0.lock().expect("not poisoned").push(revision);
        Ok(())
    }
}

/// Whether this box can render: lavapipe or another Vulkan driver. On CI
/// the answer is yes or the test fails, as the golden test does.
fn can_render() -> bool {
    match Headless::new() {
        Ok(_) => true,
        Err(error) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CI has lavapipe installed, so this is a failure, not a skip: {error}"
            );
            eprintln!("skipped: {error}");
            false
        }
    }
}

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory, no sign-in provider, and thumbnails rendered
/// within `budget` with retries recorded.
async fn server(db: PgPool, budget: Duration) -> (SocketAddr, TempDir, Arc<Recording>) {
    let dir = scratch_dir!();
    let blobs = Arc::new(FsBlobStore::new(dir.path()));
    let (addr, tasks) = serve(db, blobs, |thumbnails| thumbnails.with_budget(budget)).await;
    (addr, dir, tasks)
}

/// The same with `blobs` as the store and the thumbnails as `configure`
/// leaves them.
async fn serve(
    db: PgPool,
    blobs: Arc<dyn BlobStore>,
    configure: impl FnOnce(Thumbnails) -> Thumbnails,
) -> (SocketAddr, Arc<Recording>) {
    serve_purging(db, blobs, configure, Arc::new(NoPurge)).await
}

/// The same with `purge` taking what the writes purge.
async fn serve_purging(
    db: PgPool,
    blobs: Arc<dyn BlobStore>,
    configure: impl FnOnce(Thumbnails) -> Thumbnails,
    purge: Arc<dyn Purge>,
) -> (SocketAddr, Arc<Recording>) {
    let tasks = Arc::new(Recording::default());
    let thumbnails = configure(Thumbnails::new(Arc::clone(&tasks) as Arc<dyn TaskQueue>));
    let state = AppState::new(
        db,
        blobs,
        Arc::new(Auth::new(Vec::new(), PUBLIC_URL).expect("an auth")),
        Arc::new(thumbnails),
        purge,
    );
    (http::serve_on_free_port(state).await, tasks)
}

/// A store that keeps what it is given and cannot read it back: a render
/// that fails, as a store having a bad moment makes one fail.
struct Unreadable(FsBlobStore);

#[async_trait]
impl BlobStore for Unreadable {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        self.0.put(bytes).await
    }

    async fn get(&self, _hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        Err(Error::Failed {
            reason: "this store reads nothing".to_owned(),
        })
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        self.0.exists(hash).await
    }

    async fn delete(&self, hash: &BlobHash) -> Result<()> {
        self.0.delete(hash).await
    }

    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        self.0.get_gzip(hash).await
    }
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "thumbnailer".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("t").parse().expect("a handle");
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

/// The hand-off files pushed as a new design `midi8knob`, with `message`,
/// or `files` of them.
async fn push(browser: &mut http::Browser, design: &str, files: &[(&str, &[u8])]) -> String {
    let mut parts = vec![
        Part::text("design", design),
        Part::text("message", "pushed"),
    ];
    for (name, bytes) in files {
        parts.push(Part::file(name, bytes));
    }
    let posted = browser.post_multipart("/upload", &parts).await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    posted.header("location").expect("a location").to_owned()
}

/// The head revision a design page shows: the hash in the `<code>`'s
/// title.
fn shown_revision(body: &str) -> RevisionHash {
    let start = body.find("<code title=\"").expect("the revision's code") + "<code title=\"".len();
    body[start..start + 64].parse().expect("a hash")
}

/// The PNG at `path`: its headers checked, its pixels decoded.
async fn png(addr: SocketAddr, path: &str) -> Image {
    let response = http::send_raw(addr, "GET", path, &[], None, b"").await;
    assert_eq!(response.status, 200, "{path}");
    assert_eq!(response.header("content-type"), Some("image/png"), "{path}");
    assert_eq!(
        response.header("cache-control"),
        Some(Policy::Immutable.value()),
        "{path}"
    );
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    Image::from_png(&response.body).expect("a PNG")
}

/// The page colour as a pixel: what a thumbnail is outside the board.
fn page_colour() -> [u8; 4] {
    let kiewer_viewer::render::Rgb(r, g, b) = palette::PAGE;
    [r, g, b, 255]
}

/// A sheet with nothing on it, under a uuid of its own.
fn empty_sheet() -> String {
    format!(
        "(kicad_sch (version 20250114) (uuid \"{}\"))",
        fresh("empty")
    )
}

/// The hand-off sheet under a uuid of its own: bytes no other test
/// uploads, since a block outlives the test in the database and refuses
/// those bytes for every test after it.
fn unique_sheet() -> Vec<u8> {
    let text = std::str::from_utf8(SCHEMATIC).expect("the sheet is text");
    let uuid = "def34376-48e8-4c94-ba5b-58781196d402";
    assert!(text.contains(uuid), "the sheet's own uuid");
    text.replacen(uuid, &fresh("sheet"), 1).into_bytes()
}

/// The head of `design_path`'s default branch, off its history page: the
/// newest revision it links. Every revision has this page, whichever
/// page its design opens on.
async fn head_on_history(addr: SocketAddr, design_path: &str) -> RevisionHash {
    let history = history_page(addr, design_path).await;
    let prefix = format!("href=\"{design_path}/r/");
    let start = history.body.find(&prefix).expect("a revision is linked") + prefix.len();
    history.body[start..start + 64].parse().expect("a hash")
}

/// The retry path is 404 for a revision nobody has and for text that is
/// not a hash.
async fn assert_retry_refuses(addr: SocketAddr) {
    let nobody = http::request(
        addr,
        "POST",
        &format!("/api/thumbnails/{}", "0".repeat(64)),
        &[],
        "",
    )
    .await;
    assert_eq!(nobody.status, 404);
    let nonsense = http::request(addr, "POST", "/api/thumbnails/x", &[], "").await;
    assert_eq!(nonsense.status, 404);
}

/// `route` answers 404, and no cache may hold that.
async fn assert_gone(addr: SocketAddr, route: &str) {
    let gone = http::request(addr, "GET", route, &[], "").await;
    assert_eq!(gone.status, 404, "{route}");
    assert_eq!(
        gone.header("cache-control"),
        Some("private, no-store"),
        "{route}"
    );
}

/// The history page of `design_path`'s default branch.
async fn history_page(addr: SocketAddr, design_path: &str) -> http::Response {
    let path = format!("{design_path}/b/main/history");
    let history = http::request(addr, "GET", &path, &[], "").await;
    assert_eq!(history.status, 200, "{}", history.body);
    history
}

/// The design page `body`'s tags name the page and `revision`'s large
/// thumbnail; the branch and revision pages name their own URLs.
async fn assert_open_graph(
    addr: SocketAddr,
    design_path: &str,
    revision: RevisionHash,
    body: &str,
) {
    let large_url = format!("{PUBLIC_URL}{design_path}/r/{revision}/thumbnail/1200x630.png");
    for expected in [
        r#"<meta property="og:title" content="midi8knob">"#.to_owned(),
        format!(r#"<meta property="og:url" content="{PUBLIC_URL}{design_path}">"#),
        format!(r#"<meta property="og:image" content="{large_url}">"#),
        r#"<meta property="og:image:width" content="1200">"#.to_owned(),
        r#"<meta property="og:image:height" content="630">"#.to_owned(),
        r#"<meta name="twitter:card" content="summary_large_image">"#.to_owned(),
        r#"<meta property="og:description" content="Format version 20260206, Layers 26 (4 copper)"#
            .to_owned(),
    ] {
        assert!(body.contains(&expected), "{expected}: {body}");
    }
    let at_branch = http::request(addr, "GET", &format!("{design_path}/b/main"), &[], "").await;
    assert!(
        at_branch.body.contains(&format!(
            r#"<meta property="og:url" content="{PUBLIC_URL}{design_path}">"#
        )),
        "the default branch's head is the design's page: {}",
        at_branch.body
    );
    let at_revision =
        http::request(addr, "GET", &format!("{design_path}/r/{revision}"), &[], "").await;
    assert!(
        at_revision.body.contains(&format!(
            r#"<meta property="og:url" content="{PUBLIC_URL}{design_path}/r/{revision}">"#
        )),
        "{}",
        at_revision.body
    );
    assert!(
        at_revision.body.contains(&large_url),
        "{}",
        at_revision.body
    );
}

/// The history, person and home pages show `revision`'s small thumbnail,
/// and the history's tags carry the large one.
async fn assert_small_thumbnail_shown(
    addr: SocketAddr,
    handle: &Handle,
    design_path: &str,
    revision: RevisionHash,
) {
    let small_tag = format!(
        r#"<img class="thumb" src="{design_path}/r/{revision}/thumbnail/400x400.png" width="64" height="64""#
    );
    let history = history_page(addr, design_path).await;
    assert!(history.body.contains(&small_tag), "{}", history.body);
    assert!(
        history.body.contains(&format!(
            "{PUBLIC_URL}{design_path}/r/{revision}/thumbnail/1200x630.png"
        )),
        "the history's tags carry the head's: {}",
        history.body
    );
    assert!(
        history.body.contains(&format!(
            r#"<meta property="og:url" content="{PUBLIC_URL}{design_path}/b/main/history">"#
        )),
        "{}",
        history.body
    );
    let person = http::request(addr, "GET", &format!("/{handle}"), &[], "").await;
    assert!(person.body.contains(&small_tag), "{}", person.body);
    let (_, home) = http::get(addr, "/").await;
    assert!(home.contains(&small_tag), "{home}");
}

/// A size nobody renders, text that is not a hash and a revision that is
/// not there are 404.
async fn assert_route_refuses(addr: SocketAddr, design_path: &str, revision: RevisionHash) {
    let (status, _) = http::get(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/640x480.png"),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = http::get(
        addr,
        &format!("{design_path}/r/not-a-hash/thumbnail/400x400.png"),
    )
    .await;
    assert_eq!(status, 404);
    let zeros = "0".repeat(64);
    let (status, _) = http::get(
        addr,
        &format!("{design_path}/r/{zeros}/thumbnail/400x400.png"),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("/thumbnail/{revision}/400x400.png")).await;
    assert_eq!(status, 404, "a bare hash is no route (ADR 0002)");
}

#[tokio::test]
async fn uploading_the_hand_off_board_renders_both_sizes_and_the_pages_point_at_them() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let (addr, _dir, tasks) = server(db.clone(), Duration::from_secs(60)).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let design_path = push(
        &mut browser,
        "",
        &[
            ("midi8knob.kicad_pcb", BOARD),
            ("midi8knob.kicad_sch", SCHEMATIC),
            ("midi8knob.kicad_pro", PROJECT),
        ],
    )
    .await;
    assert_eq!(design_path, format!("/{handle}/midi8knob"));
    assert!(tasks.queued().is_empty(), "within the budget, no retry");

    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    let revision = shown_revision(&page.body);

    // Both sizes, fit to the board: the middle is board, a corner is
    // page.
    let large = png(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/1200x630.png"),
    )
    .await;
    assert_eq!((large.width, large.height), (1200, 630));
    assert_ne!(
        large.pixel(600, 315),
        Some(page_colour()),
        "the board is drawn"
    );
    assert_eq!(large.pixel(2, 2), Some(page_colour()), "the corner is page");
    let small = png(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/400x400.png"),
    )
    .await;
    assert_eq!((small.width, small.height), (400, 400));
    assert_ne!(small.pixel(200, 200), Some(page_colour()));
    assert_eq!(small.pixel(2, 2), Some(page_colour()));

    assert_open_graph(addr, &design_path, revision, &page.body).await;
    assert_small_thumbnail_shown(addr, &handle, &design_path, revision).await;
    assert_route_refuses(addr, &design_path, revision).await;

    // Another design does not reach the revision, so it has no such
    // thumbnail, though the hash is a real one.
    let sheet = empty_sheet();
    let other = push(&mut browser, "", &[("other.kicad_sch", sheet.as_bytes())]).await;
    let (status, _) = http::get(addr, &format!("{other}/r/{revision}/thumbnail/400x400.png")).await;
    assert_eq!(status, 404, "a hash is no capability (ADR 0002)");
}

#[tokio::test]
async fn a_revision_with_only_a_schematic_draws_its_sheet() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let (addr, _dir, tasks) = server(db.clone(), Duration::from_secs(60)).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let design_path = push(&mut browser, "", &[("midi8knob.kicad_sch", SCHEMATIC)]).await;
    assert!(tasks.queued().is_empty(), "within the budget, no retry");
    let revision = head_on_history(addr, &design_path).await;
    let sheet = png(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/1200x630.png"),
    )
    .await;
    assert_eq!((sheet.width, sheet.height), (1200, 630));
    assert_ne!(
        sheet.pixel(600, 315),
        Some(page_colour()),
        "paper, not the board's page colour"
    );
    let small = png(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/400x400.png"),
    )
    .await;
    assert_eq!((small.width, small.height), (400, 400));

    // The design's URL opens on the schematic page, which is the page
    // an unfurl lands on, so it carries the tags.
    let opened = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(opened.status, 303, "{}", opened.body);
    let sheet_path = format!("{design_path}/schematic");
    assert_eq!(opened.header("location"), Some(sheet_path.as_str()));
    let page = http::request(addr, "GET", &sheet_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    for expected in [
        r#"<meta property="og:title" content="midi8knob">"#.to_owned(),
        format!(r#"<meta property="og:url" content="{PUBLIC_URL}{sheet_path}">"#),
        format!(
            r#"<meta property="og:image" content="{PUBLIC_URL}{design_path}/r/{revision}/thumbnail/1200x630.png">"#
        ),
        format!(r#"<meta property="og:description" content="A KiCad design by {handle}">"#),
    ] {
        assert!(page.body.contains(&expected), "{expected}: {}", page.body);
    }
}

#[tokio::test]
async fn a_sheet_with_nothing_on_it_has_no_thumbnail_and_no_retry() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let (addr, _dir, tasks) = server(db.clone(), Duration::from_secs(60)).await;
    let (mut browser, _, _) = signed_in(&db, addr).await;
    let sheet = empty_sheet();
    let design_path = push(&mut browser, "", &[("empty.kicad_sch", sheet.as_bytes())]).await;
    assert!(tasks.queued().is_empty(), "nothing to retry");
    let revision = head_on_history(addr, &design_path).await;
    let history = history_page(addr, &design_path).await;
    assert!(
        history.body.contains(r#"class="thumb placeholder""#),
        "{}",
        history.body
    );
    let retried = http::request(
        addr,
        "POST",
        &format!("/api/thumbnails/{revision}"),
        &[],
        "",
    )
    .await;
    assert_eq!(
        (retried.status, retried.body.as_str()),
        (200, "nothing to render\n"),
        "done, so the queue stops asking"
    );
}

#[tokio::test]
async fn a_blocked_file_takes_its_thumbnail_off_the_route_and_every_page() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let (addr, _dir, _tasks) = server(db.clone(), Duration::from_secs(60)).await;
    let (mut browser, _, handle) = signed_in(&db, addr).await;
    let sheet = unique_sheet();
    let design_path = push(&mut browser, "", &[("blocked.kicad_sch", &sheet)]).await;
    let revision = head_on_history(addr, &design_path).await;
    let small_route = format!("{design_path}/r/{revision}/thumbnail/400x400.png");
    png(addr, &small_route).await;
    let small_src = format!(r#"src="{small_route}""#);
    assert!(
        history_page(addr, &design_path)
            .await
            .body
            .contains(&small_src),
        "shown before the block"
    );

    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(BlobHash::of(&sheet).as_bytes())
        .execute(&db)
        .await
        .unwrap();

    assert_gone(addr, &small_route).await;
    assert_gone(
        addr,
        &format!("{design_path}/r/{revision}/thumbnail/1200x630.png"),
    )
    .await;
    let history = history_page(addr, &design_path).await;
    assert!(!history.body.contains(&small_src), "{}", history.body);
    assert!(
        history.body.contains(r#"class="thumb placeholder""#),
        "{}",
        history.body
    );
    assert!(!history.body.contains("og:image"), "{}", history.body);
    let person = http::request(addr, "GET", &format!("/{handle}"), &[], "").await;
    assert!(!person.body.contains(&small_src), "{}", person.body);
    let stayed = http::request(addr, "GET", &format!("{design_path}?design"), &[], "").await;
    assert_eq!(stayed.status, 200, "{}", stayed.body);
    assert!(!stayed.body.contains("og:image"), "{}", stayed.body);
}

#[tokio::test]
async fn a_render_past_the_budget_saves_the_revision_queues_a_retry_and_the_placeholder_shows_until_it_lands()
 {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    // The draw takes ten times the budget, on its thread with the
    // renderer locked: the retry below waits for that lock.
    let dir = scratch_dir!();
    let (addr, tasks) = serve(
        db.clone(),
        Arc::new(FsBlobStore::new(dir.path())),
        |thumbnails| {
            thumbnails
                .with_budget(Duration::from_millis(100))
                .with_render_delay(Duration::from_secs(1))
        },
    )
    .await;
    let (mut browser, _, _) = signed_in(&db, addr).await;
    let design_path = push(&mut browser, "", &[("midi8knob.kicad_pcb", BOARD)]).await;
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "the revision is saved: {}", page.body);
    let revision = shown_revision(&page.body);
    assert_eq!(tasks.queued(), [revision], "one retry queued");

    // The placeholder: no image in the tags, the box on the history
    // page, 404 from the route and nothing cached of it.
    assert!(!page.body.contains("og:image"), "{}", page.body);
    assert!(
        page.body
            .contains(r#"<meta name="twitter:card" content="summary">"#),
        "{}",
        page.body
    );
    let history = http::request(
        addr,
        "GET",
        &format!("{design_path}/b/main/history"),
        &[],
        "",
    )
    .await;
    assert!(
        history.body.contains(r#"class="thumb placeholder""#),
        "{}",
        history.body
    );
    let missing = http::request(
        addr,
        "GET",
        &format!("{design_path}/r/{revision}/thumbnail/400x400.png"),
        &[],
        "",
    )
    .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.header("cache-control"), Some("private, no-store"));

    // The retry path renders it with no budget; a second call finds it.
    let retry_path = format!("/api/thumbnails/{revision}");
    let rendered = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(rendered.status, 200, "{}", rendered.body);
    assert_eq!(rendered.body, "rendered\n");
    let again = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(again.body, "already rendered\n");
    let (status, body) = http::get(
        addr,
        &format!(
            "{design_path}/r/{revision}/thumbnail/{}",
            Size::Small.file_name()
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert!(
        page.body.contains(&format!(
            "{design_path}/r/{revision}/thumbnail/1200x630.png"
        )),
        "{}",
        page.body
    );
    let history = http::request(
        addr,
        "GET",
        &format!("{design_path}/b/main/history"),
        &[],
        "",
    )
    .await;
    assert!(
        history.body.contains(&format!(
            r#"src="{design_path}/r/{revision}/thumbnail/400x400.png""#
        )),
        "{}",
        history.body
    );

    assert_retry_refuses(addr).await;
}

#[tokio::test]
async fn a_render_that_lands_on_the_retry_purges_every_design_holding_the_revision() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let dir = scratch_dir!();
    let purge = Arc::new(Recorder::default());
    let (addr, _tasks) = serve_purging(
        db.clone(),
        Arc::new(FsBlobStore::new(dir.path())),
        |thumbnails| {
            thumbnails
                .with_budget(Duration::from_millis(100))
                .with_render_delay(Duration::from_secs(1))
        },
        purge.clone(),
    )
    .await;
    let (mut owner, owner_id, owner_handle) = signed_in(&db, addr).await;
    let design_path = push(&mut owner, "", &[("midi8knob.kicad_pcb", BOARD)]).await;
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    let revision = shown_revision(&page.body);
    let (mut forker, forker_id, forker_handle) = signed_in(&db, addr).await;
    let made = forker
        .post_form(&format!("{design_path}/fork"), "take=default")
        .await;
    assert_eq!(made.status, 303, "{}", made.body);

    let source = design::by_owner(&db, owner_id).await.unwrap().remove(0);
    let fork = design::by_owner(&db, forker_id).await.unwrap().remove(0);
    purge.clear();
    let retry_path = format!("/api/thumbnails/{revision}");
    let rendered = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(rendered.body, "rendered\n");
    let mut expected = vec![
        Key::Home,
        Key::Design(source.id),
        Key::Design(fork.id),
        Key::Handle(owner_handle),
        Key::Handle(forker_handle),
    ];
    expected.sort_unstable();
    assert_eq!(
        purge.calls(),
        [expected],
        "the placeholder goes from every page"
    );

    purge.clear();
    let again = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(again.body, "already rendered\n");
    assert!(purge.calls().is_empty(), "nothing changed the second time");
}

#[tokio::test]
async fn a_browser_that_gives_up_during_the_render_still_gets_the_push_purged() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    if !can_render() {
        return;
    }
    let dir = scratch_dir!();
    let purge = Arc::new(Recorder::default());
    // The budget holds the request open after the commit: the window in
    // which a browser closing its tab ends the connection.
    let (addr, _tasks) = serve_purging(
        db.clone(),
        Arc::new(FsBlobStore::new(dir.path())),
        |thumbnails| {
            thumbnails
                .with_budget(Duration::from_secs(2))
                .with_render_delay(Duration::from_secs(3))
        },
        purge.clone(),
    )
    .await;
    let (mut browser, account, _) = signed_in(&db, addr).await;
    let upload = tokio::spawn(async move {
        push(&mut browser, "", &[("midi8knob.kicad_pcb", BOARD)]).await;
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while design::by_owner(&db, account).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the push commits");
    upload.abort();
    tokio::time::timeout(Duration::from_secs(8), async {
        while purge.calls().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the purge ran though the browser left");
}

#[tokio::test]
async fn a_render_that_fails_saves_the_revision_and_queues_a_retry() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let blobs = Arc::new(Unreadable(FsBlobStore::new(dir.path())));
    let (addr, tasks) = serve(db.clone(), blobs, |thumbnails| thumbnails).await;
    let (mut browser, _, _) = signed_in(&db, addr).await;
    let design_path = push(&mut browser, "", &[("midi8knob.kicad_pcb", BOARD)]).await;
    let revision = head_on_history(addr, &design_path).await;
    assert_eq!(tasks.queued(), [revision], "one retry queued");
}

/// The fake of Google's token endpoint: a token's claims by its name,
/// 400 for one it refuses, 503 for one it never got to look at.
async fn tokeninfo(Query(query): Query<HashMap<String, String>>) -> axum::response::Response {
    let claims = |aud: &str, email: &str, verified: &str| {
        Json(serde_json::json!({"aud": aud, "email": email, "email_verified": verified}))
            .into_response()
    };
    match query.get("id_token").map(String::as_str) {
        Some("good") => claims(PUBLIC_URL, TASK_ACCOUNT, "true"),
        Some("another-account") => claims(PUBLIC_URL, "stranger@example.com", "true"),
        Some("another-audience") => claims("https://elsewhere.example", TASK_ACCOUNT, "true"),
        Some("unverified") => claims(PUBLIC_URL, TASK_ACCOUNT, "false"),
        Some("refused") => StatusCode::BAD_REQUEST.into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// A token an attacker mints for themselves passes Google's own check,
/// so every claim has to hold; and an endpoint that gave no verdict has
/// refused nobody.
#[test_case("good", Some(true); "the queue's token")]
#[test_case("another-account", Some(false); "another account's token")]
#[test_case("another-audience", Some(false); "a token for another service")]
#[test_case("unverified", Some(false); "an email not verified")]
#[test_case("refused", Some(false); "a token the endpoint refuses")]
#[test_case("down", None; "no verdict from the endpoint")]
#[tokio::test]
async fn the_retry_token_is_held_to_its_claims(token: &str, verdict: Option<bool>) {
    let (listener, addr) = http::bind_free_port().await;
    let endpoint = Router::new().route("/tokeninfo", get(tokeninfo));
    tokio::spawn(async move { axum::serve(listener, endpoint).await });
    let auth = TaskAuth::oidc_at(
        &format!("http://{addr}/tokeninfo"),
        TASK_ACCOUNT,
        PUBLIC_URL,
    );
    if let Some(verdict) = verdict {
        assert_eq!(auth.allows(Some(token)).await.expect("a verdict"), verdict);
    } else {
        let error = auth.allows(Some(token)).await.expect_err("no verdict");
        assert!(matches!(error, Error::Failed { .. }), "{error}");
    }
}

#[tokio::test]
async fn the_retry_path_behind_oidc_refuses_a_caller_without_a_token() {
    let dir = scratch_dir!();
    let thumbnails = Thumbnails::new(Arc::new(Recording::default()))
        .with_auth(TaskAuth::oidc(TASK_ACCOUNT, PUBLIC_URL));
    let state = AppState::new(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
        Arc::new(Auth::new(Vec::new(), PUBLIC_URL).expect("an auth")),
        Arc::new(thumbnails),
        Arc::new(kiewer_lib::cache::NoPurge),
    );
    let addr = http::serve_on_free_port(state).await;
    let refused = http::request(
        addr,
        "POST",
        &format!("/api/thumbnails/{}", "0".repeat(64)),
        &[],
        "",
    )
    .await;
    assert_eq!(refused.status, 401, "{}", refused.body);
    assert!(refused.body.contains("token"), "{}", refused.body);
}
