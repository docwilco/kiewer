//! The renderer beacon (docs/v1-scope.md, Telemetry) through a real
//! listener: an accepted beacon is one structured JSON log line, a
//! malformed one is refused with no line, and the per-IP limit trips. The
//! log is read through the JSON formatter the server runs with on Cloud
//! Run (`kiewer/src/main.rs`), so the line asserted here is the line a
//! Cloud Logging metric reads.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use kiewer_lib::auth::Auth;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::AppState;
use kiewer_lib::web::beacon::{BEACON_PATH, LIMIT_PER_MINUTE, MAX_BODY};
use kiewer_test_support::db::fresh;
use kiewer_test_support::{TempDir, http, scratch_dir};
use test_case::test_case;

const CHROME: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// The process's log as JSON lines. A global subscriber is per process
/// and the tests of this file share one, so each test finds its own
/// lines by a marker no other test uses.
static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn log() -> Arc<Mutex<Vec<u8>>> {
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || LogWriter(Arc::clone(&writer)))
            .init();
        buffer
    })
    .clone()
}

/// Every log line so far, parsed.
fn log_lines() -> Vec<serde_json::Value> {
    let bytes = log().lock().expect("the log buffer").clone();
    String::from_utf8(bytes)
        .expect("the log is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every log line is JSON"))
        .collect()
}

/// The log lines whose text anywhere contains `marker`.
fn log_lines_with(marker: &str) -> Vec<serde_json::Value> {
    log_lines()
        .into_iter()
        .filter(|line| line.to_string().contains(marker))
        .collect()
}

async fn start() -> (SocketAddr, TempDir) {
    let _ = log();
    let dir = scratch_dir!();
    let state = AppState {
        db: kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    (http::serve_on_free_port(state).await, dir)
}

/// `POST /api/beacon` with `body` from the client `ip` (as Fastly names
/// it) and Chrome's user agent.
async fn send(addr: SocketAddr, ip: &str, body: &str) -> http::Response {
    http::request(
        addr,
        "POST",
        BEACON_PATH,
        &[("Fastly-Client-IP", ip), ("User-Agent", CHROME)],
        body,
    )
    .await
}

fn beacon(vendor: &str) -> String {
    format!(
        r#"{{"backend":"webgpu","webgpu_present":true,"outcome":"ok","reason":"","vendor":"{vendor}"}}"#
    )
}

#[tokio::test]
async fn an_accepted_beacon_is_one_structured_log_line() {
    let (addr, _dir) = start().await;
    let vendor = fresh("vendor");
    let response = send(addr, "203.0.113.10", &beacon(&vendor)).await;
    assert_eq!(response.status, 204, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    let lines = log_lines_with(&vendor);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["target"], "kiewer_lib::web::beacon");
    assert_eq!(line["backend"], "webgpu");
    assert_eq!(line["webgpu_present"], true);
    assert_eq!(line["outcome"], "ok");
    assert_eq!(line["reason"], "");
    assert_eq!(line["vendor"], vendor);
    assert_eq!(line["user_agent_family"], "chrome");
    assert!(
        line.get("ip").is_none() && !line.to_string().contains("203.0.113.10"),
        "no identifier in the line: {line}"
    );
}

#[tokio::test]
async fn a_failure_and_a_panic_carry_their_reason() {
    let (addr, _dir) = start().await;
    let reason = fresh("no adapter");
    let body = format!(
        r#"{{"backend":"none","webgpu_present":false,"outcome":"failed","reason":"{reason}"}}"#
    );
    let response = http::request(
        addr,
        "POST",
        BEACON_PATH,
        &[
            ("Fastly-Client-IP", "203.0.113.11"),
            ("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"),
        ],
        &body,
    )
    .await;
    assert_eq!(response.status, 204, "{}", response.body);
    let lines = log_lines_with(&reason);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["backend"], "none");
    assert_eq!(lines[0]["webgpu_present"], false);
    assert_eq!(lines[0]["outcome"], "failed");
    assert_eq!(lines[0]["reason"], reason);
    assert_eq!(lines[0]["vendor"], "");
    assert_eq!(lines[0]["user_agent_family"], "safari");

    let panic = fresh("panicked at kiewer_viewer/src/scene.rs:40:9");
    let body = format!(
        r#"{{"backend":"webgl2","webgpu_present":true,"outcome":"panic","reason":"{panic}","vendor":"intel"}}"#
    );
    let response = http::request(
        addr,
        "POST",
        BEACON_PATH,
        &[("Fastly-Client-IP", "203.0.113.11")],
        &body,
    )
    .await;
    assert_eq!(response.status, 204, "{}", response.body);
    let lines = log_lines_with(&panic);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["outcome"], "panic");
    assert_eq!(lines[0]["backend"], "webgl2");
    assert_eq!(lines[0]["vendor"], "intel");
    assert_eq!(
        lines[0]["user_agent_family"], "other",
        "no user agent at all"
    );
}

#[test_case("not json {m}" ; "not json")]
#[test_case(r#"{"backend":"vulkan","webgpu_present":true,"outcome":"ok","vendor":"{m}"}"# ; "a backend outside the set")]
#[test_case(r#"{"backend":"webgpu","webgpu_present":true,"outcome":"meh","vendor":"{m}"}"# ; "an outcome outside the set")]
#[test_case(r#"{"backend":"webgpu","webgpu_present":"yes","outcome":"ok","vendor":"{m}"}"# ; "a string where a boolean goes")]
#[test_case(r#"{"webgpu_present":true,"outcome":"ok","vendor":"{m}"}"# ; "a missing field")]
#[test_case(r#"{"backend":"webgpu","webgpu_present":true,"outcome":"ok","vendor":"{m}","session":"abc"}"# ; "an unknown field")]
#[test_case(r#"["{m}"]"# ; "an array")]
#[tokio::test]
async fn a_malformed_beacon_is_refused_and_not_logged(body: &str) {
    let (addr, _dir) = start().await;
    let marker = fresh("marker");
    let body = body.replace("{m}", &marker);
    let response = send(addr, "203.0.113.20", &body).await;
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    assert!(
        log_lines_with(&marker).is_empty(),
        "nothing logged for a refused beacon"
    );
}

#[tokio::test]
async fn a_reason_or_a_vendor_over_its_bound_is_refused() {
    let (addr, _dir) = start().await;
    let marker = fresh("marker");
    let long_reason = format!("{marker}{}", "x".repeat(1024));
    let body = format!(
        r#"{{"backend":"webgpu","webgpu_present":true,"outcome":"panic","reason":"{long_reason}"}}"#
    );
    assert_eq!(send(addr, "203.0.113.21", &body).await.status, 400);
    let long_vendor = format!("{marker}{}", "v".repeat(64));
    assert_eq!(
        send(addr, "203.0.113.21", &beacon(&long_vendor))
            .await
            .status,
        400
    );
    assert!(log_lines_with(&marker).is_empty());
    // Exactly at the bound is accepted.
    let at_bound = format!("{marker}{}", "x".repeat(1024 - marker.len()));
    let body = format!(
        r#"{{"backend":"webgpu","webgpu_present":true,"outcome":"panic","reason":"{at_bound}"}}"#
    );
    assert_eq!(send(addr, "203.0.113.21", &body).await.status, 204);
    assert_eq!(log_lines_with(&marker).len(), 1);
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let (addr, _dir) = start().await;
    let marker = fresh("marker");
    let body = format!(r#"{{"reason":"{marker}{}"#, "x".repeat(MAX_BODY));
    let response = send(addr, "203.0.113.22", &body).await;
    assert_eq!(response.status, 413, "{}", response.body);
    assert!(log_lines_with(&marker).is_empty());
}

#[tokio::test]
async fn the_limit_trips_per_client_and_leaves_other_clients_alone() {
    let (addr, _dir) = start().await;
    let vendor = fresh("vendor");
    for n in 0..LIMIT_PER_MINUTE {
        let response = send(addr, "203.0.113.77", &beacon(&vendor)).await;
        assert_eq!(response.status, 204, "beacon {n}: {}", response.body);
    }
    let refused = send(addr, "203.0.113.77", &beacon(&vendor)).await;
    assert_eq!(refused.status, 429, "{}", refused.body);
    assert_eq!(refused.header("retry-after"), Some("60"));
    assert_eq!(refused.header("cache-control"), Some("private, no-store"));
    let other = send(addr, "203.0.113.78", &beacon(&vendor)).await;
    assert_eq!(other.status, 204, "{}", other.body);
    let lines = log_lines_with(&vendor);
    assert_eq!(
        lines.len(),
        LIMIT_PER_MINUTE as usize + 1,
        "the refused beacon is not logged"
    );
}

#[tokio::test]
async fn without_a_fastly_header_the_last_forwarded_for_entry_is_the_client() {
    let (addr, _dir) = start().await;
    let vendor = fresh("vendor");
    // The first entry is whatever the client sent; the last is what the
    // front end appended. A client varying the first must not escape.
    for n in 0..LIMIT_PER_MINUTE {
        let spoof = format!("198.51.100.{n}, 203.0.113.90");
        let response = http::request(
            addr,
            "POST",
            BEACON_PATH,
            &[("X-Forwarded-For", &spoof)],
            &beacon(&vendor),
        )
        .await;
        assert_eq!(response.status, 204, "beacon {n}");
    }
    let response = http::request(
        addr,
        "POST",
        BEACON_PATH,
        &[("X-Forwarded-For", "198.51.100.200, 203.0.113.90")],
        &beacon(&vendor),
    )
    .await;
    assert_eq!(response.status, 429);
}

#[tokio::test]
async fn only_post_is_served() {
    let (addr, _dir) = start().await;
    let (status, _) = http::get(addr, BEACON_PATH).await;
    assert_eq!(status, 405);
}
