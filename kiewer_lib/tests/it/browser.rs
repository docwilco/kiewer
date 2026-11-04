//! The pages in a real browser. `cargo test` serves a page and reads its
//! bytes, so a page that serves but fails once its script runs passes
//! every other test; here chrome-headless-shell (the version
//! `scripts/browser.sh` pins) loads it over the Chrome DevTools Protocol
//! and draws WebGL2 on SwiftShader, with no GPU. The board page's first
//! load is checked through the network, its beacon answered by the
//! server; the canvas cases of #94 below read the beacons the page hands
//! to `sendBeacon`, through the server's parser.
//!
//! Runs when `KIEWER_BROWSER` names the browser's executable, which the
//! browser stage of `scripts/checks.sh` sets, along with
//! `KIEWER_VIEWER_DIST` so the server carries the viewer
//! (`kiewer_lib/build.rs`). Without it the test skips with a line saying
//! so, as it does in the test stage.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, EventResponseReceived, GetRequestPostDataParams, RequestId,
};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt as _;
use kiewer_assert::assert_is_empty;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::beacon::{BEACON_PATH, Backend, Beacon, Outcome};
use kiewer_lib::web::viewer::embedded;
use kiewer_test_support::{TempDir, http, scratch_dir};
use serde::Deserialize;
use test_case::test_case;
use tokio::sync::{Semaphore, SemaphorePermit};

/// How long a page gets to send its beacon: SwiftShader draws on the CPU
/// and the viewer is a debug build, so this is generous.
const DEADLINE: Duration = Duration::from_secs(60);

/// How long reading what a page shows may take, so a page stuck in its
/// script still lets the failure say what it was waiting for.
const READ_DEADLINE: Duration = Duration::from_secs(5);

/// The browser's executable, or `None` with a line saying the test
/// skips.
fn executable() -> Option<PathBuf> {
    let path = std::env::var_os("KIEWER_BROWSER").filter(|path| !path.is_empty());
    if path.is_none() {
        eprintln!("KIEWER_BROWSER is unset; skipping (scripts/checks.sh browser sets it)");
    }
    path.map(PathBuf::from)
}

async fn start_server() -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
    );
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
    assert!(
        embedded(),
        "KIEWER_BROWSER is set but this server has no viewer in it; \
         KIEWER_VIEWER_DIST names the viewer to embed (scripts/checks.sh browser sets both)"
    );
    let (addr, _blobs) = start_server().await;
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

/// The canvas's displayed width in the capped cases, in CSS pixels, and
/// its height: short, so the 4x MSAA target at the limit stays a few
/// hundred megabytes on SwiftShader.
const CAPPED_WIDTH: u32 = 1200;
const CAPPED_HEIGHT: u32 = 140;

/// Room for the page's header line above the canvas, in CSS pixels.
const HEADER: u32 = 40;

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

impl Session {
    /// A browser on a blank tab, recording beacons, with `probe` setting
    /// data-panic-probe on the canvases of whatever page loads next.
    async fn new(executable: PathBuf, probe: bool) -> Session {
        let permit = BROWSERS.acquire().await.expect("the semaphore stays open");
        let (addr, blobs) = start_server().await;
        let profile = scratch_dir!();
        let (browser, traffic) = launch(executable, &profile).await;
        let page = browser.new_page("about:blank").await.expect("a tab");
        page.evaluate_on_new_document(RECORD_BEACONS)
            .await
            .expect("the beacon recorder");
        if probe {
            page.evaluate_on_new_document(PANIC_PROBE)
                .await
                .expect("the panic probe");
        }
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

    async fn goto(&self, path: &str) {
        self.page
            .goto(format!("http://{}{path}", self.addr))
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

    /// Waits until `condition` holds on the page, up to [`DEADLINE`];
    /// whether it did.
    async fn until(&self, condition: &str) -> bool {
        let poll = async {
            while !self.read::<bool>(condition).await {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::time::timeout(DEADLINE, poll).await.is_ok()
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

    async fn canvas(&self, id: &str) -> Canvas {
        self.read(&format!(
            r#"(() => {{
                const canvas = document.getElementById("{id}");
                const r = canvas.getBoundingClientRect();
                return {{ width: canvas.width, height: canvas.height, cssWidth: r.width, cssHeight: r.height }};
            }})()"#
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

    async fn outcomes(&self) -> Vec<Outcome> {
        self.beacons()
            .await
            .iter()
            .map(|beacon| beacon.outcome)
            .collect()
    }

    /// What the page shows, waiting for it to answer.
    async fn shown(&self) -> Shown {
        shown(&self.page).await.expect("the page answers")
    }

    async fn close(mut self) {
        self.browser.close().await.expect("the browser closes");
        self.browser.wait().await.expect("the browser exits");
        self.traffic.await.expect("the protocol task ends");
    }
}

/// The browser's executable once the server is known to carry the
/// viewer, or `None` when the test skips.
fn ready() -> Option<PathBuf> {
    let executable = executable()?;
    assert!(
        embedded(),
        "KIEWER_BROWSER is set but this server has no viewer in it; \
         KIEWER_VIEWER_DIST names the viewer to embed (scripts/checks.sh browser sets both)"
    );
    Some(executable)
}

/// The device pixel ratio that puts a canvas [`CAPPED_WIDTH`] CSS pixels
/// wide a tenth past `limit`.
fn dpr_past(limit: u32) -> u32 {
    (limit + limit / 10).div_ceil(CAPPED_WIDTH)
}

#[test_case("/viewer", "board"; "the board page")]
#[test_case("/schematic", "sheet"; "the schematic page")]
#[tokio::test]
async fn a_canvas_2100_wide_draws_at_full_size_on_webgl2(path: &str, id: &str) {
    let Some(executable) = ready() else {
        return;
    };
    let session = Session::new(executable, false).await;
    session.viewport(2100, 900 + HEADER, 1).await;
    session.goto(path).await;
    let sent = session.until("window.kiewerBeacons.length > 0").await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    assert!(
        sent,
        "no beacon within {DEADLINE:?}; the page shows {shown:?}"
    );

    let canvas = session.canvas(id).await;
    assert_eq!(canvas.width, 2100, "{canvas:?}");
    assert!(
        shown.status.starts_with("webgl2"),
        "the backend line: {shown:?}"
    );
    assert_eq!(shown.error, None, "the error box stays hidden");
    assert_eq!(
        session.outcomes().await,
        [Outcome::Ok],
        "one ok beacon, no capped one"
    );
    session.close().await;
}

#[test_case("/viewer", "board"; "the board page")]
#[test_case("/schematic", "sheet"; "the schematic page")]
#[tokio::test]
async fn a_canvas_past_the_adapters_limit_is_capped_with_its_aspect_kept(path: &str, id: &str) {
    let Some(executable) = ready() else {
        return;
    };
    let session = Session::new(executable, false).await;
    let limit = session.texture_limit().await;
    let dpr = dpr_past(limit);
    session
        .viewport(CAPPED_WIDTH, CAPPED_HEIGHT + HEADER, dpr)
        .await;
    session.goto(path).await;
    let sent = session.until("window.kiewerBeacons.length >= 2").await;
    tokio::time::sleep(SETTLE).await;
    let shown = session.shown().await;
    assert!(
        sent,
        "no ok and capped beacons within {DEADLINE:?}; the page shows {shown:?}, sent {:?}",
        session.beacons().await
    );

    let canvas = session.canvas(id).await;
    let wanted = (
        (canvas.css_width * f64::from(dpr)).round(),
        (canvas.css_height * f64::from(dpr)).round(),
    );
    assert!(
        wanted.0 > f64::from(limit),
        "the page asks past {limit}: {canvas:?}"
    );
    assert_eq!(
        canvas.width, limit,
        "the longer side at the limit: {canvas:?}"
    );
    let kept = (wanted.1 * f64::from(limit) / wanted.0).round();
    assert!(
        (f64::from(canvas.height) - kept).abs() <= 1.0,
        "the aspect is kept: {canvas:?}, asked {wanted:?}"
    );
    assert!(
        shown.status.starts_with("webgl2"),
        "the backend line: {shown:?}"
    );
    assert_eq!(shown.error, None, "the error box stays hidden");
    let beacons = session.beacons().await;
    assert_eq!(
        beacons
            .iter()
            .map(|beacon| beacon.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Ok, Outcome::Capped],
        "{beacons:?}"
    );
    assert_eq!(beacons[1].backend, Backend::WebGl2, "{beacons:?}");
    assert_is_empty!(beacons[1].reason);
    session.close().await;
}

#[test_case("/viewer", "board"; "the board page")]
#[test_case("/schematic", "sheet"; "the schematic page")]
#[tokio::test]
async fn a_window_that_grows_past_the_limit_after_startup_is_capped_once(path: &str, id: &str) {
    let Some(executable) = ready() else {
        return;
    };
    let session = Session::new(executable, false).await;
    let limit = session.texture_limit().await;
    let dpr = dpr_past(limit);
    let under = limit / dpr - 200;
    session.viewport(under, CAPPED_HEIGHT + HEADER, dpr).await;
    session.goto(path).await;
    let sent = session.until("window.kiewerBeacons.length > 0").await;
    let shown = session.shown().await;
    assert!(
        sent,
        "no beacon within {DEADLINE:?}; the page shows {shown:?}"
    );
    let canvas = session.canvas(id).await;
    assert!(
        canvas.width < limit,
        "under the limit at startup: {canvas:?}"
    );

    // Past the limit, back under it and past it again: one capped beacon.
    for width in [CAPPED_WIDTH, under, CAPPED_WIDTH + 100] {
        session.viewport(width, CAPPED_HEIGHT + HEADER, dpr).await;
        let follows = if width == under {
            format!(r#"document.getElementById("{id}").width < {limit}"#)
        } else {
            format!(r#"document.getElementById("{id}").width === {limit}"#)
        };
        assert!(
            session.until(&follows).await,
            "the canvas follows the window to {width}: {:?}",
            session.canvas(id).await
        );
    }
    tokio::time::sleep(SETTLE).await;

    let canvas = session.canvas(id).await;
    assert_eq!(canvas.width, limit, "capped after growing: {canvas:?}");
    let shown = session.shown().await;
    assert_eq!(shown.error, None, "no panic: {shown:?}");
    assert_eq!(
        session.outcomes().await,
        [Outcome::Ok, Outcome::Capped],
        "one capped beacon over two crossings, no panic"
    );
    session.close().await;
}

#[test_case("/viewer"; "the board page")]
#[test_case("/schematic"; "the schematic page")]
#[tokio::test]
async fn a_panic_inside_viewer_create_shows_and_sends_its_beacon(path: &str) {
    let Some(executable) = ready() else {
        return;
    };
    let session = Session::new(executable, true).await;
    session.viewport(1000, 700, 1).await;
    session.goto(path).await;
    let failed = session
        .until(r#"!document.getElementById("error").hidden"#)
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
    assert_eq!(
        beacons
            .iter()
            .map(|beacon| beacon.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Panic],
        "{beacons:?}"
    );
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
