//! The pages in a real browser. `cargo test` serves a page and reads its
//! bytes, so a page that serves but fails once its script runs passes
//! every other test; here chrome-headless-shell (the version
//! `scripts/browser.sh` pins) loads it over the Chrome DevTools Protocol
//! and draws WebGL2 on SwiftShader, with no GPU.
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

use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, EventResponseReceived, GetRequestPostDataParams, RequestId,
};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures_util::StreamExt as _;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::beacon::{BEACON_PATH, Backend, Beacon, Outcome};
use kiewer_lib::web::viewer::embedded;
use kiewer_test_support::{TempDir, http, scratch_dir};
use serde::Deserialize;

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
