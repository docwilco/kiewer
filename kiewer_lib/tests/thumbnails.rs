//! Thumbnails and Open Graph over HTTP (#25, ADR 0004): uploading the
//! hand-off board renders both sizes inside the request and the design,
//! branch, revision and history pages point at them; a render past the
//! budget leaves the revision saved with a retry queued and the pages on
//! the placeholder until the retry path renders it; a revision with only
//! a schematic draws its sheet. The Postgres tests skip without
//! `DATABASE_URL`, like `tests/db.rs`, and the renders skip on a box with
//! no Vulkan driver, as the viewer's golden test does; on CI (`CI` set)
//! both are failures, since the runner has Postgres and lavapipe.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiewer_lib::Result;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Auth, Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::revision::RevisionHash;
use kiewer_lib::session;
use kiewer_lib::thumbnail::{Size, TaskQueue, Thumbnails};
use kiewer_lib::web::AppState;
use kiewer_lib::web::blob::IMMUTABLE;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use kiewer_viewer::headless::Headless;
use kiewer_viewer::render::offscreen::Image;
use kiewer_viewer::view::palette;
use sqlx::PgPool;

const BOARD: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
const PROJECT: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");

/// The public URL the test's sign-in flow is built with, which the Open
/// Graph tags' absolute URLs start with.
const PUBLIC_URL: &str = "http://127.0.0.1:1";

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
    let tasks = Arc::new(Recording::default());
    let thumbnails = Thumbnails::new(Arc::clone(&tasks) as Arc<dyn TaskQueue>).with_budget(budget);
    let state = AppState::new(
        db,
        Arc::new(FsBlobStore::new(dir.path())),
        Arc::new(Auth::new(Vec::new(), PUBLIC_URL).expect("an auth")),
        Arc::new(thumbnails),
    );
    (http::serve_on_free_port(state).await, dir, tasks)
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
    assert_eq!(response.header("cache-control"), Some(IMMUTABLE), "{path}");
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    Image::from_png(&response.body).expect("a PNG")
}

/// The page colour as a pixel: what a thumbnail is outside the board.
fn page_colour() -> [u8; 4] {
    let kiewer_viewer::render::Rgb(r, g, b) = palette::PAGE;
    [r, g, b, 255]
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
    let large_url = format!("{PUBLIC_URL}/thumbnail/{revision}/1200x630.png");
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
        r#"<img class="thumb" src="/thumbnail/{revision}/400x400.png" width="64" height="64""#
    );
    let history = history_page(addr, design_path).await;
    assert!(history.body.contains(&small_tag), "{}", history.body);
    assert!(
        history
            .body
            .contains(&format!("{PUBLIC_URL}/thumbnail/{revision}/1200x630.png")),
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
async fn assert_route_refuses(addr: SocketAddr, revision: RevisionHash) {
    let (status, _) = http::get(addr, &format!("/thumbnail/{revision}/640x480.png")).await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, "/thumbnail/not-a-hash/400x400.png").await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, &format!("/thumbnail/{}/400x400.png", "0".repeat(64))).await;
    assert_eq!(status, 404);
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
    let large = png(addr, &format!("/thumbnail/{revision}/1200x630.png")).await;
    assert_eq!((large.width, large.height), (1200, 630));
    assert_ne!(
        large.pixel(600, 315),
        Some(page_colour()),
        "the board is drawn"
    );
    assert_eq!(large.pixel(2, 2), Some(page_colour()), "the corner is page");
    let small = png(addr, &format!("/thumbnail/{revision}/400x400.png")).await;
    assert_eq!((small.width, small.height), (400, 400));
    assert_ne!(small.pixel(200, 200), Some(page_colour()));
    assert_eq!(small.pixel(2, 2), Some(page_colour()));

    assert_open_graph(addr, &design_path, revision, &page.body).await;
    assert_small_thumbnail_shown(addr, &handle, &design_path, revision).await;
    assert_route_refuses(addr, revision).await;
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
    let (mut browser, _, _) = signed_in(&db, addr).await;
    let design_path = push(&mut browser, "", &[("midi8knob.kicad_sch", SCHEMATIC)]).await;
    assert!(tasks.queued().is_empty(), "within the budget, no retry");
    let revision = head_on_history(addr, &design_path).await;
    let sheet = png(addr, &format!("/thumbnail/{revision}/1200x630.png")).await;
    assert_eq!((sheet.width, sheet.height), (1200, 630));
    assert_ne!(
        sheet.pixel(600, 315),
        Some(page_colour()),
        "paper, not the board's page colour"
    );
    let small = png(addr, &format!("/thumbnail/{revision}/400x400.png")).await;
    assert_eq!((small.width, small.height), (400, 400));
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
    let (addr, _dir, tasks) = server(db.clone(), Duration::ZERO).await;
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
        &format!("/thumbnail/{revision}/400x400.png"),
        &[],
        "",
    )
    .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.header("cache-control"), Some("no-store"));

    // The retry path renders it with no budget; a second call finds it.
    let retry_path = format!("/api/thumbnails/{revision}");
    let rendered = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(rendered.status, 200, "{}", rendered.body);
    assert_eq!(rendered.body, "rendered\n");
    let again = http::request(addr, "POST", &retry_path, &[], "").await;
    assert_eq!(again.body, "already rendered\n");
    let (status, body) = http::get(
        addr,
        &format!("/thumbnail/{revision}/{}", Size::Small.file_name()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    assert!(
        page.body
            .contains(&format!("/thumbnail/{revision}/1200x630.png")),
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
        history
            .body
            .contains(&format!(r#"src="/thumbnail/{revision}/400x400.png""#)),
        "{}",
        history.body
    );

    // A revision nobody has, and text that is not a hash.
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

#[tokio::test]
async fn the_retry_path_behind_oidc_refuses_a_caller_without_a_token() {
    let dir = scratch_dir!();
    let thumbnails = Thumbnails::new(Arc::new(Recording::default())).with_auth(
        kiewer_lib::thumbnail::TaskAuth::oidc("kiewer-run@p.iam.gserviceaccount.com", PUBLIC_URL),
    );
    let state = AppState::new(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
        Arc::new(Auth::new(Vec::new(), PUBLIC_URL).expect("an auth")),
        Arc::new(thumbnails),
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
