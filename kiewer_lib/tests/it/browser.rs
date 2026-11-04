//! The pages in a real browser. `cargo test` serves a page and reads its
//! bytes, so a page that serves but fails once its script runs passes
//! every other test; here chrome-headless-shell (the version
//! `scripts/browser.sh` pins) loads it over the Chrome DevTools Protocol
//! and draws WebGL2 on SwiftShader, with no GPU. The board page's first
//! load is checked through the network, its beacon answered by the
//! server; the canvas cases of #94 and the failed import of #96 below
//! read the beacons the page hands to `sendBeacon`, through the server's
//! parser. The net block's case (#95) reads its computed display before
//! and after the load, and the failed import's case reads it once the
//! viewer has failed to start. The upload page's zips (#83) are chosen
//! in its file input and either land as a revision or are denied on the
//! page; those cases need Postgres too, as does the upload form's
//! design dropdown (#62), which reloads the form with the branches of the
//! design picked.
//!
//! Runs when `KIEWER_BROWSER` names the browser's executable, which the
//! browser stage of `scripts/checks.sh` sets, along with
//! `KIEWER_VIEWER_DIST` so the server carries the viewer
//! (`kiewer_lib/build.rs`). Without it the test skips with a line saying
//! so, as it does in the test stage.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, EventRequestPaused, FulfillRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{
    CookieParam, EventRequestWillBeSent, EventResponseReceived, GetRequestPostDataParams, RequestId,
};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::{Browser, BrowserConfig, Page};
use flate2::write::DeflateEncoder;
use flate2::{Compression, Crc};
use futures_util::StreamExt as _;
use kiewer_assert::assert_is_empty;
use kiewer_lib::account::Handle;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::design;
use kiewer_lib::session;
use kiewer_lib::web::beacon::{BEACON_PATH, Backend, Beacon, Outcome};
use kiewer_lib::web::viewer::{embedded, script_url};
use kiewer_test_support::db::pool_from_env;
use kiewer_test_support::{TempDir, http, scratch_dir};
use serde::Deserialize;
use sqlx::PgPool;
use test_case::test_case;
use tokio::sync::{Semaphore, SemaphorePermit};

use super::upload::{BOARD, LOWER_SHEET, PROJECT, SCHEMATIC, listed_paths, person};

/// How long a page gets to send its beacon: SwiftShader draws on the CPU
/// and the viewer is a debug build, so this is generous.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long reading what a page shows may take, so a page stuck in its
/// script still lets the failure say what it was waiting for.
const READ_DEADLINE: Duration = Duration::from_secs(5);

/// The browser's executable, or `None` with a line saying the test
/// skips. A browser with no viewer in the server is a broken setup, not
/// a skip, so it fails here.
fn executable() -> Option<PathBuf> {
    let path = std::env::var_os("KIEWER_BROWSER").filter(|path| !path.is_empty());
    if path.is_none() {
        eprintln!("KIEWER_BROWSER is unset; skipping (scripts/checks.sh browser sets it)");
    }
    let path = path.map(PathBuf::from)?;
    assert!(
        embedded(),
        "KIEWER_BROWSER is set but this server has no viewer in it; \
         KIEWER_VIEWER_DIST names the viewer to embed (scripts/checks.sh browser sets both)"
    );
    Some(path)
}

/// A pool that never connects, for the pages that read no database.
fn no_database() -> PgPool {
    kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool")
}

async fn start_server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// The browser on a profile of its own under `profile`, and the task
/// that carries its protocol traffic.
async fn launch(executable: PathBuf, profile: &TempDir) -> (Browser, tokio::task::JoinHandle<()>) {
    let config = BrowserConfig::builder()
        .chrome_executable(executable)
        .user_data_dir(profile.path())
        // The page is the test's own, from loopback; the sandbox needs
        // user namespaces a container or a hardened runner may refuse.
        .no_sandbox()
        // WebGL2 with no GPU: ANGLE on SwiftShader, which Chrome
        // otherwise refuses as unsafe, and no blocklist in front of it.
        .arg(("use-angle", "swiftshader"))
        .arg("enable-unsafe-swiftshader")
        .arg("ignore-gpu-blocklist")
        .build()
        .expect("a browser config");
    let (browser, mut handler) = Browser::launch(config).await.expect("the browser starts");
    let traffic = tokio::spawn(async move { while handler.next().await.is_some() {} });
    (browser, traffic)
}

/// What the page shows: the backend line and, when it is not hidden, the
/// error box.
#[derive(Debug, Deserialize)]
struct Shown {
    status: String,
    error: Option<String>,
}

/// What `page` shows, or `None` when it does not answer within
/// [`READ_DEADLINE`].
async fn shown(page: &Page) -> Option<Shown> {
    let read = page.evaluate(
        r#"({
            status: document.getElementById("status").textContent,
            error: document.getElementById("error").hidden
                ? null
                : document.getElementById("error").textContent,
        })"#,
    );
    let shown = tokio::time::timeout(READ_DEADLINE, read).await.ok()?;
    Some(
        shown
            .expect("the page evaluates")
            .into_value()
            .expect("the status and the error box"),
    )
}

/// A beacon as the server answered it: what the page posted, read by the
/// server's own parser, and the status that came back.
#[derive(Debug)]
struct Answered {
    beacon: Beacon,
    status: i64,
}

/// The first beacon the page sends, once its response is in. The two
/// events come on separate streams, so either may be read first.
async fn first_beacon(
    page: &Page,
    requests: &mut EventStream<EventRequestWillBeSent>,
    responses: &mut EventStream<EventResponseReceived>,
) -> Answered {
    let mut sent: Option<(RequestId, Beacon)> = None;
    let mut answered: HashMap<RequestId, i64> = HashMap::new();
    loop {
        tokio::select! {
            Some(event) = requests.next() => {
                if sent.is_none() && event.request.url.ends_with(BEACON_PATH) {
                    let body = page
                        .execute(GetRequestPostDataParams::new(event.request_id.clone()))
                        .await
                        .expect("the beacon's body")
                        .result
                        .post_data;
                    let beacon = Beacon::parse(body.as_bytes()).expect("a beacon the server takes");
                    sent = Some((event.request_id.clone(), beacon));
                }
            }
            Some(event) = responses.next() => {
                if event.response.url.ends_with(BEACON_PATH) {
                    answered.insert(event.request_id.clone(), event.response.status);
                }
            }
            else => panic!("the browser's event streams ended before a beacon was answered"),
        }
        if let Some((id, beacon)) = &sent
            && let Some(&status) = answered.get(id)
        {
            return Answered {
                beacon: beacon.clone(),
                status,
            };
        }
    }
}

#[tokio::test]
async fn the_board_page_draws_on_webgl2_and_its_ok_beacon_arrives() {
    let Some(executable) = executable() else {
        return;
    };
    let (addr, _blobs) = start_server(no_database()).await;
    let profile = scratch_dir!();
    let (mut browser, traffic) = launch(executable, &profile).await;

    let page = browser.new_page("about:blank").await.expect("a tab");
    let mut requests = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .expect("request events");
    let mut responses = page
        .event_listener::<EventResponseReceived>()
        .await
        .expect("response events");
    page.goto(format!("http://{addr}/viewer"))
        .await
        .expect("the page loads");

    let beacon =
        tokio::time::timeout(DEADLINE, first_beacon(&page, &mut requests, &mut responses)).await;
    let shown = shown(&page).await;
    let answered = beacon
        .unwrap_or_else(|_| panic!("no beacon within {DEADLINE:?}; the page shows {shown:?}"));
    let shown = shown.expect("the page answers once its beacon is sent");

    assert!(
        shown.status.starts_with("webgl2"),
        "the backend line: {shown:?}, beacon {answered:?}"
    );
    assert_eq!(shown.error, None, "the error box stays hidden");
    assert_eq!(answered.beacon.outcome, Outcome::Ok, "{answered:?}");
    assert_eq!(answered.beacon.backend, Backend::WebGl2, "{answered:?}");
    assert_eq!(answered.status, 204, "the server took it: {answered:?}");

    browser.close().await.expect("the browser closes");
    browser.wait().await.expect("the browser exits");
    traffic.await.expect("the protocol task ends");
}

// The canvas cases (#94), on the board page and the schematic page alike:
// a canvas over the WebGL2 floor's 2048 draws at full size, one over the
// adapter's own limit is capped with its aspect kept and says so once, a
// window that grows past the limit after startup does not panic, and a
// real panic inside Viewer.create shows and sends. The viewer is the
// stage's debug build with panic-probe on (scripts/checks.sh browser).

/// At most this many of the canvas cases hold a browser at once: each
/// draws on SwiftShader, on the CPU, and the capped ones draw a canvas
/// thousands of pixels wide with 4x MSAA.
static BROWSERS: Semaphore = Semaphore::const_new(2);

/// How long a page gets to settle after its last beacon or resize before
/// the beacons are counted, so one sent late is counted too.
const SETTLE: Duration = Duration::from_millis(1500);

/// The canvas's displayed width in the capped cases, in CSS pixels.
const CAPPED_WIDTH: u32 = 1200;

/// The viewport's height in the capped cases, in CSS pixels: the page's
/// header line and a short canvas under it, so the 4x MSAA target at the
/// limit stays a few hundred megabytes on SwiftShader.
const CAPPED_VIEWPORT_HEIGHT: u32 = 180;

/// The viewport's height for a canvas 900 CSS pixels tall: the canvas and
/// room for the page's header line above it.
const FULL_VIEWPORT_HEIGHT: u32 = 940;

/// Records each beacon's body as the page hands it to sendBeacon, before
/// any script of the page runs.
const RECORD_BEACONS: &str = r"
window.kiewerBeacons = [];
const kiewerSend = navigator.sendBeacon.bind(navigator);
navigator.sendBeacon = function (url, body) {
  window.kiewerBeacons.push(String(body));
  return kiewerSend(url, body);
};
";

/// Marks every canvas with data-panic-probe once the document is parsed,
/// which is before the page's module scripts run.
const PANIC_PROBE: &str = r#"
document.addEventListener("readystatechange", function () {
  for (const canvas of document.querySelectorAll("canvas")) canvas.setAttribute("data-panic-probe", "");
});
"#;

/// The two pages that draw on a canvas with the viewer.
#[derive(Debug, Clone, Copy)]
enum Viewed {
    Board,
    Schematic,
}

impl Viewed {
    fn path(self) -> &'static str {
        match self {
            Viewed::Board => "/viewer",
            Viewed::Schematic => "/schematic",
        }
    }

    /// The id of the page's canvas.
    fn canvas(self) -> &'static str {
        match self {
            Viewed::Board => "board",
            Viewed::Schematic => "sheet",
        }
    }
}

/// A page in a browser of its own, with the server behind it.
struct Session {
    browser: Browser,
    traffic: tokio::task::JoinHandle<()>,
    page: Page,
    addr: SocketAddr,
    _blobs: TempDir,
    _profile: TempDir,
    _permit: SemaphorePermit<'static>,
}

/// The canvas as the page holds it: its backing size and its displayed
/// size in CSS pixels.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Canvas {
    width: u32,
    height: u32,
    css_width: f64,
    css_height: f64,
}

/// The outcome of each beacon, in the order they were sent.
fn outcomes(beacons: &[Beacon]) -> Vec<Outcome> {
    beacons.iter().map(|beacon| beacon.outcome).collect()
}

impl Session {
    /// A browser on a blank tab that records the beacons of whatever page
    /// loads next.
    async fn new(executable: PathBuf) -> Session {
        Session::over(executable, no_database(), RECORD_BEACONS).await
    }

    /// A browser on a blank tab over a server on `db`, with `recorder`
    /// run before any script of whatever page loads next.
    async fn over(executable: PathBuf, db: PgPool, recorder: &str) -> Session {
        let permit = BROWSERS.acquire().await.expect("the semaphore stays open");
        let (addr, blobs) = start_server(db).await;
        let profile = scratch_dir!();
        let (browser, traffic) = launch(executable, &profile).await;
        let page = browser.new_page("about:blank").await.expect("a tab");
        page.evaluate_on_new_document(recorder)
            .await
            .expect("the recorder");
        Session {
            browser,
            traffic,
            page,
            addr,
            _blobs: blobs,
            _profile: profile,
            _permit: permit,
        }
    }

    /// As [`Session::new`], with data-panic-probe set on the canvases of
    /// the page that loads next, so Viewer.create panics there.
    async fn with_panic_probe(executable: PathBuf) -> Session {
        let session = Session::new(executable).await;
        session
            .page
            .evaluate_on_new_document(PANIC_PROBE)
            .await
            .expect("the panic probe");
        session
    }

    /// As [`Session::new`], with the net block's display recorded once
    /// the page that loads next is parsed, in `window.kiewerNetAtParse`.
    async fn with_net_recorder(executable: PathBuf) -> Session {
        let session = Session::new(executable).await;
        session
            .page
            .evaluate_on_new_document(NET_AT_PARSE)
            .await
            .expect("the net block's recorder");
        session
    }

    /// Has the browser answer every request for `path` with a 404 and no
    /// body, before it reaches the server. The count is how many it
    /// answered so.
    async fn answer_not_found(&self, path: &str) -> Arc<AtomicUsize> {
        let mut paused = self
            .page
            .event_listener::<EventRequestPaused>()
            .await
            .expect("paused request events");
        self.page
            .execute(
                fetch::EnableParams::builder()
                    .pattern(
                        RequestPattern::builder()
                            .url_pattern(format!("*{path}"))
                            .build(),
                    )
                    .build(),
            )
            .await
            .expect("request interception");
        let answered = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&answered);
        let page = self.page.clone();
        // Ends with the browser, when the event stream does. A request it
        // fails to answer stays paused, so the failure is said here.
        tokio::spawn(async move {
            while let Some(event) = paused.next().await {
                match page
                    .execute(FulfillRequestParams::new(event.request_id.clone(), 404))
                    .await
                {
                    Ok(_) => {
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) => eprintln!("answering {} with a 404: {error}", event.request.url),
                }
            }
        });
        answered
    }

    /// The viewport, `width` by `height` CSS pixels at `dpr` device
    /// pixels each; on a loaded page this is a resize.
    async fn viewport(&self, width: u32, height: u32, dpr: u32) {
        self.page
            .execute(SetDeviceMetricsOverrideParams::new(
                i64::from(width),
                i64::from(height),
                f64::from(dpr),
                false,
            ))
            .await
            .expect("the viewport");
    }

    /// Loads `viewed`'s page from the server.
    async fn goto(&self, viewed: Viewed) {
        self.page
            .goto(format!("http://{}{}", self.addr, viewed.path()))
            .await
            .expect("the page loads");
    }

    /// The value of `expression` on the page.
    async fn read<T: serde::de::DeserializeOwned>(&self, expression: &str) -> T {
        self.page
            .evaluate(expression)
            .await
            .expect("the page evaluates")
            .into_value()
            .expect("a value of the expected shape")
    }

    /// Whether `condition` came to hold on the page within [`DEADLINE`].
    async fn held_within_deadline(&self, condition: &str) -> bool {
        let poll = async {
            while !self.read::<bool>(condition).await {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(DEADLINE, poll).await.is_ok()
    }

    /// Whether the page sent `count` beacons within [`DEADLINE`].
    async fn sent_within_deadline(&self, count: usize) -> bool {
        self.held_within_deadline(&format!("window.kiewerBeacons.length >= {count}"))
            .await
    }

    /// WebGL2's `MAX_TEXTURE_SIZE`, which wgpu reads as the adapter's
    /// `max_texture_dimension_2d`.
    async fn texture_limit(&self) -> u32 {
        self.read(
            r#"document.createElement("canvas").getContext("webgl2")
                .getParameter(WebGL2RenderingContext.MAX_TEXTURE_SIZE)"#,
        )
        .await
    }

    /// `viewed`'s canvas as the page holds it now.
    async fn canvas(&self, viewed: Viewed) -> Canvas {
        self.read(&format!(
            r#"(() => {{
                const canvas = document.getElementById("{}");
                const r = canvas.getBoundingClientRect();
                return {{ width: canvas.width, height: canvas.height, cssWidth: r.width, cssHeight: r.height }};
            }})()"#,
            viewed.canvas()
        ))
        .await
    }

    /// Whether `viewed`'s canvas came to be `width` backing pixels wide
    /// within [`DEADLINE`].
    async fn canvas_within_deadline(&self, viewed: Viewed, width: u32) -> bool {
        self.held_within_deadline(&format!(
            r#"document.getElementById("{}").width === {width}"#,
            viewed.canvas()
        ))
        .await
    }

    /// Every beacon the page has sent, read by the server's own parser.
    async fn beacons(&self) -> Vec<Beacon> {
        let bodies: Vec<String> = self.read("window.kiewerBeacons").await;
        bodies
            .iter()
            .map(|body| Beacon::parse(body.as_bytes()).expect("a beacon the server takes"))
            .collect()
    }

    /// The net block's computed display: `none` while it is not shown.
    async fn net_display(&self) -> String {
        self.read(r#"getComputedStyle(document.getElementById("net")).display"#)
            .await
    }

    /// What the page shows, waiting for it to answer.
    async fn shown(&self) -> Shown {
        shown(&self.page).await.expect("the page answers")
    }

    /// Closes the browser and waits for it and its protocol task to end.
    async fn close(mut self) {
        self.browser.close().await.expect("the browser closes");
        self.browser.wait().await.expect("the browser exits");
        self.traffic.await.expect("the protocol task ends");
    }
}

/// The device pixel ratio that puts a canvas [`CAPPED_WIDTH`] CSS pixels
/// wide a tenth past `limit`.
fn dpr_past(limit: u32) -> u32 {
    (limit + limit / 10).div_ceil(CAPPED_WIDTH)
}

#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn a_canvas_2100_wide_draws_at_full_size_on_webgl2(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::new(executable).await;
    session.viewport(2100, FULL_VIEWPORT_HEIGHT, 1).await;
    session.goto(viewed).await;
    let sent = session.sent_within_deadline(1).await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    assert!(
        sent,
        "no beacon within {DEADLINE:?}; the page shows {shown:?}"
    );

    let canvas = session.canvas(viewed).await;
    assert_eq!(canvas.width, 2100, "{canvas:?}");
    assert!(
        shown.status.starts_with("webgl2"),
        "the backend line: {shown:?}"
    );
    assert_eq!(shown.error, None, "the error box stays hidden");
    assert_eq!(
        outcomes(&session.beacons().await),
        [Outcome::Ok],
        "one ok beacon, no capped one"
    );
    session.close().await;
}

#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn a_canvas_past_the_adapters_limit_is_capped_with_its_aspect_kept(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::new(executable).await;
    let limit = session.texture_limit().await;
    let dpr = dpr_past(limit);
    session
        .viewport(CAPPED_WIDTH, CAPPED_VIEWPORT_HEIGHT, dpr)
        .await;
    session.goto(viewed).await;
    let sent = session.sent_within_deadline(2).await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    let beacons = session.beacons().await;
    assert!(
        sent,
        "no ok and capped beacons within {DEADLINE:?}; the page shows {shown:?}, sent {beacons:?}"
    );

    let canvas = session.canvas(viewed).await;
    let asked = (
        (canvas.css_width * f64::from(dpr)).round(),
        (canvas.css_height * f64::from(dpr)).round(),
    );
    assert!(
        asked.0 > f64::from(limit),
        "the page asks past {limit}: {canvas:?}"
    );
    assert_eq!(
        canvas.width, limit,
        "the longer side at the limit: {canvas:?}"
    );
    let aspect_kept = (asked.1 * f64::from(limit) / asked.0).round();
    assert!(
        (f64::from(canvas.height) - aspect_kept).abs() <= 1.0,
        "the aspect is kept: {canvas:?}, asked {asked:?}"
    );
    assert!(
        shown.status.starts_with("webgl2"),
        "the backend line: {shown:?}"
    );
    assert_eq!(shown.error, None, "the error box stays hidden");
    assert_eq!(
        outcomes(&beacons),
        [Outcome::Ok, Outcome::Capped],
        "{beacons:?}"
    );
    assert_eq!(beacons[1].backend, Backend::WebGl2, "{beacons:?}");
    assert_is_empty!(beacons[1].reason);
    session.close().await;
}

#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn a_window_that_grows_past_the_limit_after_startup_is_capped_once(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::new(executable).await;
    let limit = session.texture_limit().await;
    let dpr = dpr_past(limit);
    let start_width = limit / dpr - 200;
    session
        .viewport(start_width, CAPPED_VIEWPORT_HEIGHT, dpr)
        .await;
    session.goto(viewed).await;
    let sent = session.sent_within_deadline(1).await;
    let shown = session.shown().await;
    assert!(
        sent,
        "no beacon within {DEADLINE:?}; the page shows {shown:?}"
    );
    let canvas = session.canvas(viewed).await;
    assert!(
        canvas.width < limit,
        "under the limit at startup: {canvas:?}"
    );

    // Past the limit, back to where it started and past it again, each
    // step once the canvas has followed the last: one capped beacon.
    let started = canvas.width;
    for (width, backing) in [
        (CAPPED_WIDTH, limit),
        (start_width, started),
        (CAPPED_WIDTH + 100, limit),
    ] {
        session.viewport(width, CAPPED_VIEWPORT_HEIGHT, dpr).await;
        assert!(
            session.canvas_within_deadline(viewed, backing).await,
            "the canvas follows the window to {width}: {:?}",
            session.canvas(viewed).await
        );
    }
    tokio::time::sleep(SETTLE).await;

    let canvas = session.canvas(viewed).await;
    assert!(
        canvas.css_width * f64::from(dpr) > f64::from(limit),
        "the page asks past {limit}: {canvas:?}"
    );
    assert_eq!(canvas.width, limit, "capped after growing: {canvas:?}");
    let shown = session.shown().await;
    assert_eq!(shown.error, None, "no panic: {shown:?}");
    assert_eq!(
        outcomes(&session.beacons().await),
        [Outcome::Ok, Outcome::Capped],
        "one capped beacon over two crossings, no panic"
    );
    session.close().await;
}

#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn a_panic_inside_viewer_create_shows_and_sends_its_beacon(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::with_panic_probe(executable).await;
    session.viewport(1000, 700, 1).await;
    session.goto(viewed).await;
    let failed = session
        .held_within_deadline(r#"!document.getElementById("error").hidden"#)
        .await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    assert!(
        failed,
        "no error within {DEADLINE:?}; the page shows {shown:?}"
    );

    let error = shown.error.expect("the error box shows");
    assert!(
        error.starts_with("panicked at") && error.contains("panic-probe"),
        "the panic's own message: {error}"
    );
    let beacons = session.beacons().await;
    assert_eq!(outcomes(&beacons), [Outcome::Panic], "{beacons:?}");
    assert_eq!(
        beacons[0].backend,
        Backend::None,
        "no viewer yet: {beacons:?}"
    );
    assert_eq!(
        beacons[0].reason, error,
        "the beacon carries what the box shows"
    );
    session.close().await;
}

// A viewer script that fails to load (#96): the page script imports it
// inside start's try, so the failure shows in the error box and sends one
// failed beacon, where a static import stopped the whole module first and
// the page stayed blank.
#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn a_viewer_script_that_answers_404_shows_and_sends_its_beacon(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::new(executable).await;
    let script = script_url();
    let answered = session.answer_not_found(&script).await;
    session.viewport(1000, 700, 1).await;
    session.goto(viewed).await;
    let failed = session
        .held_within_deadline(r#"!document.getElementById("error").hidden"#)
        .await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    let answered = answered.load(Ordering::SeqCst);
    assert!(
        failed,
        "no error within {DEADLINE:?}; the page shows {shown:?}, {answered} requests answered 404"
    );

    assert!(answered > 0, "the browser answered {script} with a 404");
    let error = shown.error.expect("the error box shows");
    assert!(
        error.starts_with("import: ") && error.contains(&script),
        "the failed import, naming the script: {error}"
    );
    let beacons = session.beacons().await;
    assert_eq!(outcomes(&beacons), [Outcome::Failed], "{beacons:?}");
    assert_eq!(beacons[0].backend, Backend::None, "no viewer: {beacons:?}");
    assert_eq!(
        beacons[0].reason, error,
        "the beacon carries what the box shows"
    );
    assert_eq!(
        session.net_display().await,
        "none",
        "the net block stays hidden with no viewer (#95)"
    );
    session.close().await;
}

// The net block (#95): the pinned nets and the reference search box,
// which each page shows once its board or sheet has loaded. Its rule
// sets display only while it is not hidden, so the hidden attribute it
// carries in the page holds until then.

/// Records the net block's computed display once the document is parsed,
/// which is before the page's module scripts run.
const NET_AT_PARSE: &str = r#"
document.addEventListener("readystatechange", function () {
  if (document.readyState !== "interactive") return;
  window.kiewerNetAtParse = getComputedStyle(document.getElementById("net")).display;
});
"#;

#[test_case(Viewed::Board; "the board page")]
#[test_case(Viewed::Schematic; "the schematic page")]
#[tokio::test]
async fn the_net_block_shows_only_once_the_page_has_loaded(viewed: Viewed) {
    let Some(executable) = executable() else {
        return;
    };
    let session = Session::with_net_recorder(executable).await;
    session.viewport(1000, 700, 1).await;
    session.goto(viewed).await;
    let unhidden = session
        .held_within_deadline(r#"!document.getElementById("net").hidden"#)
        .await;
    let shown = session.shown().await;
    assert!(
        unhidden,
        "the net block not unhidden within {DEADLINE:?}; the page shows {shown:?}"
    );

    let at_parse: String = session.read("window.kiewerNetAtParse").await;
    assert_eq!(at_parse, "none", "hidden before the page has loaded");
    assert_eq!(
        session.net_display().await,
        "flex",
        "displayed as flex once loaded"
    );
    session.close().await;
}

// The upload page's zips (#83, ADR 0005): the page reads a zip's central
// directory, checks it against the caps and the skip list, inflates only
// the entries it keeps and sends them as files with their paths in the
// zip; or it denies the zip on the page and sends nothing. The form is a
// signed-in person's, so these need Postgres as well and skip without
// `DATABASE_URL`.

/// Records the file names of the body the upload page posts, before any
/// script of the page runs: in session storage, so the record outlives
/// the redirect to the design page.
const RECORD_UPLOAD: &str = r#"
const kiewerFetch = window.fetch.bind(window);
window.fetch = function (url, init) {
  if (init && init.body instanceof FormData) {
    const sent = [];
    for (const [, value] of init.body.entries()) if (value instanceof File) sent.push(value.name);
    sessionStorage.setItem("kiewerSent", JSON.stringify(sent));
  }
  return kiewerFetch(url, init);
};
"#;

/// One entry of a zip as a writer put it down. Past `bytes` are what the
/// denied cases change.
#[derive(Debug, Clone, Copy)]
struct Entry<'a> {
    path: &'a str,
    bytes: &'a [u8],
    /// 0 is stored and 8 deflated; anything else is another method, its
    /// bytes left as they are.
    method: u16,
    encrypted: bool,
    /// The uncompressed size the headers declare when it is not the true
    /// one: what a zip bomb's directory does.
    declared: Option<u32>,
    /// The CRC-32 the headers declare when it is not the true one: what a
    /// zip damaged after it was written holds.
    crc: Option<u32>,
}

impl<'a> Entry<'a> {
    fn deflated(path: &'a str, bytes: &'a [u8]) -> Self {
        Entry {
            path,
            bytes,
            method: 8,
            encrypted: false,
            declared: None,
            crc: None,
        }
    }

    fn stored(path: &'a str, bytes: &'a [u8]) -> Self {
        Entry {
            method: 0,
            ..Entry::deflated(path, bytes)
        }
    }

    fn folder(path: &'a str) -> Self {
        Entry::stored(path, b"")
    }
}

/// How a zip as a whole is written.
#[derive(Debug, Clone, Copy, Default)]
struct Shape {
    /// Each entry's sizes in a data descriptor after its bytes, the local
    /// header's left zero (flag bit 3): what a streaming writer such as
    /// macOS's Archive Utility writes.
    streamed: bool,
    /// A zip64 end record and its locator before the end record, as
    /// `zip -fz` writes.
    zip64_end: bool,
    /// Each entry's sizes and offset as 0xFFFFFFFF in the directory, the
    /// true ones in a zip64 extra field.
    zip64_entries: bool,
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u32_of(len: usize) -> u32 {
    u32::try_from(len).expect("a test zip under 4 GB")
}

/// A zip of `entries`, written by hand so a case can write what no zip
/// writer at hand will: zip64 records, an encrypted flag, a size that lies.
fn zip_of(entries: &[Entry<'_>], shape: Shape) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let packed = if entry.method == 8 {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(entry.bytes).expect("deflated");
            encoder.finish().expect("deflated")
        } else {
            entry.bytes.to_vec()
        };
        let mut crc = Crc::new();
        crc.update(entry.bytes);
        let crc = entry.crc.unwrap_or(crc.sum());
        let packed_size = u32_of(packed.len());
        let size = entry.declared.unwrap_or(u32_of(entry.bytes.len()));
        let flags = u16::from(entry.encrypted) | if shape.streamed { 0b1000 } else { 0 };
        let offset = u32_of(out.len());
        let name = entry.path.as_bytes();
        let name_len = u16::try_from(name.len()).expect("a short name");

        out.extend_from_slice(b"PK\x03\x04");
        put16(&mut out, 20);
        put16(&mut out, flags);
        put16(&mut out, entry.method);
        put16(&mut out, 0);
        put16(&mut out, 0x21);
        for value in if shape.streamed {
            [0; 3]
        } else {
            [crc, packed_size, size]
        } {
            put32(&mut out, value);
        }
        put16(&mut out, name_len);
        put16(&mut out, 0);
        out.extend_from_slice(name);
        out.extend_from_slice(&packed);
        if shape.streamed {
            out.extend_from_slice(b"PK\x07\x08");
            for value in [crc, packed_size, size] {
                put32(&mut out, value);
            }
        }

        let mut extra = Vec::new();
        let listed = if shape.zip64_entries {
            put16(&mut extra, 0x0001);
            put16(&mut extra, 24);
            put64(&mut extra, u64::from(size));
            put64(&mut extra, u64::from(packed_size));
            put64(&mut extra, u64::from(offset));
            [u32::MAX; 3]
        } else {
            [packed_size, size, offset]
        };
        directory.extend_from_slice(b"PK\x01\x02");
        put16(&mut directory, 20);
        put16(&mut directory, 20);
        put16(&mut directory, flags);
        put16(&mut directory, entry.method);
        put16(&mut directory, 0);
        put16(&mut directory, 0x21);
        put32(&mut directory, crc);
        put32(&mut directory, listed[0]);
        put32(&mut directory, listed[1]);
        put16(&mut directory, name_len);
        put16(
            &mut directory,
            u16::try_from(extra.len()).expect("a short extra field"),
        );
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        put32(&mut directory, 0);
        put32(&mut directory, listed[2]);
        directory.extend_from_slice(name);
        directory.extend_from_slice(&extra);
    }
    let count = u16::try_from(entries.len()).expect("fewer than 65 536 entries");
    end_records(&mut out, &directory, count, shape);
    out
}

/// `directory` and the end records after it, at the end of `out`.
fn end_records(out: &mut Vec<u8>, directory: &[u8], count: u16, shape: Shape) {
    let start = out.len();
    out.extend_from_slice(directory);
    if shape.zip64_end {
        let record = out.len();
        out.extend_from_slice(b"PK\x06\x06");
        put64(out, 44);
        put16(out, 45);
        put16(out, 45);
        put32(out, 0);
        put32(out, 0);
        put64(out, u64::from(count));
        put64(out, u64::from(count));
        put64(out, directory.len() as u64);
        put64(out, start as u64);
        out.extend_from_slice(b"PK\x06\x07");
        put32(out, 0);
        put64(out, record as u64);
        put32(out, 1);
    }
    out.extend_from_slice(b"PK\x05\x06");
    put16(out, 0);
    put16(out, 0);
    put16(out, count);
    put16(out, count);
    put32(out, u32_of(directory.len()));
    put32(out, u32_of(start));
    put16(out, 0);
}

/// The hand-off project's four files, named `names`, each written as
/// `entry` writes it.
fn project<'a>(
    names: &'a [String; 4],
    entry: fn(&'a str, &'a [u8]) -> Entry<'a>,
) -> [Entry<'a>; 4] {
    [
        entry(&names[0], PROJECT),
        entry(&names[1], BOARD),
        entry(&names[2], SCHEMATIC),
        entry(&names[3], LOWER_SHEET),
    ]
}

/// The hand-off project's file names under `prefix`, the lower sheet in
/// `sub` with `separator` between them.
fn project_names(prefix: &str, separator: char) -> [String; 4] {
    [
        format!("{prefix}midi8knob.kicad_pro"),
        format!("{prefix}midi8knob.kicad_pcb"),
        format!("{prefix}midi8knob.kicad_sch"),
        format!("{prefix}sub{separator}power.kicad_sch"),
    ]
}

/// Explorer's "Compressed (zipped) folder" of the project folder: the
/// folder wraps everything, deflated, with files that are not KiCad
/// files beside the project; and the lower sheet's path written with a
/// backslash, as Windows PowerShell's Compress-Archive writes one.
fn explorer_zip() -> Vec<u8> {
    let names = project_names("midi8knob/", '\\');
    let mut entries = vec![Entry::folder("midi8knob/"), Entry::folder("midi8knob/sub/")];
    entries.extend(project(&names, Entry::deflated));
    entries.push(Entry::deflated("midi8knob/fp-info-cache", b"0\n"));
    entries.push(Entry::deflated("midi8knob/README.md", b"# not sent\n"));
    zip_of(&entries, Shape::default())
}

/// The project stored, not deflated, by a streaming writer: the sizes in
/// data descriptors, so only the directory has them.
fn streamed_stored_zip() -> Vec<u8> {
    let names = project_names("", '/');
    zip_of(
        &project(&names, Entry::stored),
        Shape {
            streamed: true,
            ..Shape::default()
        },
    )
}

/// The project beside entries the skip list names, each one an entry the
/// page would deny if it looked at it: encrypted, in another method, or
/// declared over a cap.
fn zip_with_skipped_folders() -> Vec<u8> {
    let names = project_names("", '/');
    let mut entries = project(&names, Entry::deflated).to_vec();
    entries.extend([
        Entry {
            encrypted: true,
            ..Entry::deflated("midi8knob-backups/midi8knob.kicad_pcb", BOARD)
        },
        Entry {
            declared: Some(60 << 20),
            ..Entry::stored(".git/huge.kicad_pcb", b"(garbage")
        },
        Entry {
            method: 12,
            ..Entry::stored("__MACOSX/._midi8knob.kicad_pcb", b"\x00\x05\x16\x07")
        },
        Entry {
            declared: Some(210 << 20),
            ..Entry::stored("_autosave-midi8knob.kicad_sch", b"(garbage")
        },
        Entry {
            method: 12,
            ..Entry::stored("midi8knob-save.kicad_pcb", b"(garbage")
        },
    ]);
    zip_of(&entries, Shape::default())
}

/// A signed-in person's upload form in a browser of its own, with the
/// server and its database behind it.
struct Uploader {
    session: Session,
    handle: Handle,
    /// The session's id, the cookie's value.
    session_id: String,
    zips: TempDir,
}

impl Uploader {
    /// The upload form, loaded for a new person with a live session.
    async fn new(executable: PathBuf, db: PgPool) -> Uploader {
        let uploader = Uploader::signed_in(executable, db).await;
        uploader.open("/upload").await;
        uploader
    }

    /// A new person with a live session in the browser, on a blank tab.
    async fn signed_in(executable: PathBuf, db: PgPool) -> Uploader {
        let session = Session::over(executable, db.clone(), RECORD_UPLOAD).await;
        let (account_id, handle) = person(&db).await;
        let session_id = session::start(&db, account_id)
            .await
            .expect("a session")
            .to_string();
        let mut cookie = CookieParam::new("kiewer_session", session_id.clone());
        cookie.url = Some(format!("http://{}/", session.addr));
        session
            .page
            .set_cookie(cookie)
            .await
            .expect("the session cookie");
        Uploader {
            session,
            handle,
            session_id,
            zips: scratch_dir!(),
        }
    }

    /// Loads `path` in the browser.
    async fn open(&self, path: &str) {
        self.session
            .page
            .goto(format!("http://{}{path}", self.session.addr))
            .await
            .expect("the page loads");
    }

    /// The same person as a plain HTTP client, for what the test sets up
    /// before the form opens.
    fn client(&self) -> http::Browser {
        let mut client = http::Browser::new(self.session.addr);
        client.set_cookie("kiewer_session", &self.session_id);
        client
    }

    /// `bytes` chosen as the zip `name` in the form's file input, and the
    /// form submitted.
    async fn upload_zip(&self, name: &str, bytes: &[u8]) {
        let path = self.zips.path().join(name);
        std::fs::write(&path, bytes).expect("the zip written");
        let page = &self.session.page;
        let input = page
            .find_element(r#"input[type="file"]:not([webkitdirectory])"#)
            .await
            .expect("the file input");
        let mut chosen = SetFileInputFilesParams::new(vec![path.display().to_string()]);
        chosen.node_id = Some(input.node_id);
        page.execute(chosen).await.expect("the zip chosen");
        page.evaluate(r#"document.querySelector("form").requestSubmit()"#)
            .await
            .expect("the form submitted");
    }

    /// Whether `condition` came to hold on the page within [`DEADLINE`]. A
    /// page between two documents does not evaluate, and counts as not
    /// yet.
    async fn held_within_deadline(&self, condition: &str) -> bool {
        let poll = async {
            loop {
                let held = self
                    .session
                    .page
                    .evaluate(condition)
                    .await
                    .ok()
                    .and_then(|result| result.into_value::<bool>().ok());
                if held == Some(true) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(DEADLINE, poll).await.is_ok()
    }

    /// The file names the page posted, or `None` when it posted nothing.
    /// The record is read as its JSON text, `null` when there is none,
    /// since the protocol hands back no value for a null.
    async fn sent(&self) -> Option<Vec<String>> {
        let record: String = self
            .read(r#"sessionStorage.getItem("kiewerSent") ?? "null""#)
            .await;
        serde_json::from_str(&record).expect("null or the names")
    }

    /// The value of `expression` on the page.
    async fn read<T: serde::de::DeserializeOwned>(&self, expression: &str) -> T {
        self.session.read(expression).await
    }
}

#[test_case(explorer_zip, "midi8knob/"; "a deflated zip with a wrapper folder and a backslash")]
#[test_case(streamed_stored_zip, ""; "a stored zip with data descriptors")]
#[test_case(zip_with_skipped_folders, ""; "a zip with skipped folders, none of them read")]
#[tokio::test]
async fn the_upload_page_expands_a_zip_and_sends_the_kicad_files_in_it(
    zip: fn() -> Vec<u8>,
    prefix: &str,
) {
    let Some(executable) = executable() else {
        return;
    };
    let Some(db) = pool_from_env().await else {
        return;
    };
    let uploader = Uploader::new(executable, db).await;
    uploader.upload_zip("midi8knob.zip", &zip()).await;
    let design_path = format!("/{}/midi8knob", uploader.handle);
    let landed = uploader
        .held_within_deadline(&format!(r#"location.pathname === "{design_path}""#))
        .await;
    let denied: String = uploader
        .read(r#"document.getElementById("denied")?.textContent ?? """#)
        .await;
    assert!(
        landed,
        "not on {design_path} within {DEADLINE:?}; the page denies {denied:?}"
    );

    assert_eq!(
        uploader.sent().await,
        Some(project_names(prefix, '/').to_vec()),
        "the KiCad files alone, with their paths in the zip, and no zip"
    );
    let page = http::request(uploader.session.addr, "GET", &design_path, &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        listed_paths(&page.body),
        [
            "midi8knob.kicad_pcb",
            "midi8knob.kicad_pro",
            "midi8knob.kicad_sch",
            "sub/power.kicad_sch"
        ],
        "rooted at the project folder, the sheet's path kept: {}",
        page.body
    );
    assert!(
        page.body.contains("<dt>Footprints</dt><dd>78</dd>"),
        "the board came out of the zip whole: {}",
        page.body
    );
    uploader.session.close().await;
}

/// The project zipped with its four files each changed by `change`.
fn project_zip_with(shape: Shape, change: fn(Entry<'_>) -> Entry<'_>) -> Vec<u8> {
    let names = project_names("", '/');
    let entries = project(&names, Entry::deflated).map(change);
    zip_of(&entries, shape)
}

fn zip64_end_records() -> Vec<u8> {
    project_zip_with(
        Shape {
            zip64_end: true,
            ..Shape::default()
        },
        |entry| entry,
    )
}

fn zip64_entries() -> Vec<u8> {
    project_zip_with(
        Shape {
            zip64_entries: true,
            ..Shape::default()
        },
        |entry| entry,
    )
}

fn encrypted_board() -> Vec<u8> {
    project_zip_with(Shape::default(), |entry| Entry {
        encrypted: entry.path.ends_with(".kicad_pcb"),
        ..entry
    })
}

fn board_in_bzip2() -> Vec<u8> {
    project_zip_with(Shape::default(), |entry| {
        if entry.path.ends_with(".kicad_pcb") {
            Entry {
                method: 12,
                ..entry
            }
        } else {
            entry
        }
    })
}

/// A board whose directory declares 60 MB: inflated, it would be denied
/// for holding less than it declares, so the cap's denial shows it never
/// was.
fn board_over_the_file_cap() -> Vec<u8> {
    project_zip_with(Shape::default(), |entry| {
        if entry.path.ends_with(".kicad_pcb") {
            Entry {
                declared: Some(60 << 20),
                ..entry
            }
        } else {
            entry
        }
    })
}

fn board_with_a_wrong_checksum() -> Vec<u8> {
    project_zip_with(Shape::default(), |entry| {
        if entry.path.ends_with(".kicad_pcb") {
            Entry {
                crc: Some(0x1234_5678),
                ..entry
            }
        } else {
            entry
        }
    })
}

/// The project and one more sheet, each declaring 45 MB: under the file
/// cap, 225 MB together.
fn files_over_the_revision_cap() -> Vec<u8> {
    let names = project_names("", '/');
    let mut entries = project(&names, Entry::deflated).to_vec();
    entries.push(Entry::deflated("extra.kicad_sch", LOWER_SHEET));
    for entry in &mut entries {
        entry.declared = Some(45 << 20);
    }
    zip_of(&entries, Shape::default())
}

/// The project behind 9,999 folders: 10,003 entries.
fn too_many_entries() -> Vec<u8> {
    let folders: Vec<String> = (0..9_999).map(|n| format!("f{n}/")).collect();
    let names = project_names("", '/');
    let mut entries: Vec<Entry<'_>> = folders.iter().map(|path| Entry::folder(path)).collect();
    entries.extend(project(&names, Entry::deflated));
    zip_of(&entries, Shape::default())
}

#[test_case(zip64_end_records, "midi8knob.zip: has zip64 records, which the page does not read; zip the project again, or drop the project folder instead"; "zip64 end records")]
#[test_case(zip64_entries, "midi8knob.zip: has zip64 records, which the page does not read; zip the project again, or drop the project folder instead"; "zip64 sizes in the entries")]
#[test_case(encrypted_board, "midi8knob.zip: midi8knob.kicad_pcb is encrypted; zip the project again, or drop the project folder instead"; "an encrypted entry")]
#[test_case(board_in_bzip2, "midi8knob.zip: midi8knob.kicad_pcb is compressed with neither deflate nor store; zip the project again, or drop the project folder instead"; "an entry in bzip2")]
#[test_case(board_with_a_wrong_checksum, "midi8knob.zip: midi8knob.kicad_pcb does not match its checksum; zip the project again, or drop the project folder instead"; "an entry that does not match its checksum")]
#[test_case(board_over_the_file_cap, "midi8knob.zip: midi8knob.kicad_pcb is larger than 50 MB, the limit for one file"; "an entry over the file cap")]
#[test_case(files_over_the_revision_cap, "the files together are larger than 200 MB, the limit for one revision"; "entries over the revision cap")]
#[test_case(too_many_entries, "midi8knob.zip: lists 10003 entries, more than the 10000 the page reads; zip the project again, or drop the project folder instead"; "more entries than the page reads")]
#[tokio::test]
async fn the_upload_page_denies_a_zip_it_will_not_expand_and_sends_nothing(
    zip: fn() -> Vec<u8>,
    denial: &str,
) {
    let Some(executable) = executable() else {
        return;
    };
    let Some(db) = pool_from_env().await else {
        return;
    };
    let uploader = Uploader::new(executable, db).await;
    uploader.upload_zip("midi8knob.zip", &zip()).await;
    let denied = uploader
        .held_within_deadline(r#"!document.getElementById("denied").hidden"#)
        .await;
    assert!(denied, "no denial within {DEADLINE:?}");

    let shown: String = uploader
        .read(r#"document.getElementById("denied").textContent"#)
        .await;
    assert_eq!(shown, denial);
    assert_eq!(uploader.sent().await, None, "nothing was posted");
    let (path, enabled): (String, bool) = uploader
        .read(r#"[location.pathname, !document.querySelector("form button").disabled]"#)
        .await;
    assert_eq!(
        (path.as_str(), enabled),
        ("/upload", true),
        "still on the form, ready for another try"
    );
    uploader.session.close().await;
}

/// A design of the uploader's named `name`, one sheet on `main`, with
/// `branches` made from that head: its path (`handle/slug`) and the head.
async fn design_with(
    db: &PgPool,
    uploader: &Uploader,
    name: &str,
    branches: &[&str],
) -> (String, String) {
    let mut client = uploader.client();
    let sheet = format!("(kicad_sch (version 20250114) (generator \"{name}\"))");
    let file_name = format!("{name}.kicad_sch");
    let pushed = client
        .post_multipart(
            "/upload",
            &[
                http::Part::text("name", name),
                http::Part::file(&file_name, sheet.as_bytes()),
            ],
        )
        .await;
    assert_eq!(pushed.status, 303, "{}", pushed.body);
    let at = pushed.header("location").expect("where it landed");
    let slug = at.rsplit('/').next().expect("a slug");
    let (design, _) = design::find(db, uploader.handle.as_str(), slug)
        .await
        .expect("a read")
        .expect("the design");
    let head = design::branches(db, design.id).await.expect("a list")[0]
        .head
        .to_string();
    let path = format!("{}/{}", uploader.handle, design.slug);
    for branch in branches {
        let made = client
            .post_form(
                &format!("/{path}/branches"),
                &format!("name={branch}&from={head}"),
            )
            .await;
        assert_eq!(made.status, 303, "{}", made.body);
    }
    (path, head)
}

/// What the upload form offers and holds: the design picked, the
/// branches offered (the empty value is "A new branch"), the branch
/// picked, and the revision it carries.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct UploadForm {
    design: String,
    branches: Vec<String>,
    branch: String,
    from: String,
}

const UPLOAD_FORM: &str = r#"(() => {
  const form = document.querySelector("form");
  return {
    design: form.elements.design.value,
    branches: [...form.elements.branch.options].map((option) => option.value),
    branch: form.elements.branch.value,
    from: form.elements.from.value,
  };
})()"#;

/// The design dropdown set to `design` as a person picks it, and the
/// form that loads for it.
async fn pick_design(uploader: &Uploader, design: &str) -> UploadForm {
    uploader
        .session
        .page
        .evaluate(format!(
            r#"{{
  const picker = document.querySelector('select[name="design"]');
  picker.value = "{design}";
  picker.dispatchEvent(new Event("change", {{ bubbles: true }}));
}}"#
        ))
        .await
        .expect("the design picked");
    let reloaded = uploader
        .held_within_deadline(&format!(
            r#"new URLSearchParams(location.search).get("design") === "{design}" && document.readyState === "complete""#
        ))
        .await;
    let at: String = uploader.read("location.href").await;
    assert!(
        reloaded,
        "not reloaded for {design:?} within {DEADLINE:?}; at {at}"
    );
    uploader.read(UPLOAD_FORM).await
}

#[tokio::test]
async fn the_upload_forms_branches_follow_the_design_picked() {
    let Some(executable) = executable() else {
        return;
    };
    let Some(db) = pool_from_env().await else {
        return;
    };
    let uploader = Uploader::signed_in(executable, db.clone()).await;
    let (alpha, alpha_head) = design_with(&db, &uploader, "alpha", &["old-a", "wip"]).await;
    let (beta, _) = design_with(&db, &uploader, "beta", &["rev-b", "wip"]).await;
    uploader
        .open(&format!(
            "/upload?design={alpha}&branch=wip&from={alpha_head}"
        ))
        .await;
    assert_eq!(
        uploader.read::<UploadForm>(UPLOAD_FORM).await,
        UploadForm {
            design: alpha.clone(),
            branches: vec!["main".into(), "old-a".into(), "wip".into(), String::new()],
            branch: "wip".into(),
            from: alpha_head,
        },
        "opened from alpha's wip"
    );

    assert_eq!(
        pick_design(&uploader, &beta).await,
        UploadForm {
            design: beta,
            branches: vec!["main".into(), "rev-b".into(), "wip".into(), String::new()],
            branch: "wip".into(),
            from: String::new(),
        },
        "beta's branches, the branch of the same name kept, alpha's revision left behind"
    );
    assert_eq!(
        pick_design(&uploader, "").await,
        UploadForm {
            design: String::new(),
            branches: vec!["main".into(), String::new()],
            branch: "main".into(),
            from: String::new(),
        },
        "a new design has main alone"
    );
    uploader.session.close().await;
}
