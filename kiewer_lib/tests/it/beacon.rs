//! The renderer beacon (docs/v1-scope.md, Telemetry) through a real
//! listener: an accepted beacon is one structured JSON log line, a
//! malformed one is refused with no line, and the per-IP limit trips. The
//! log is read through the layer the server's JSON log runs on Cloud Run
//! (`tracing_stackdriver`, installed in `kiewer/src/log.rs`), so the line
//! asserted here is the line a Cloud Logging metric reads, and the
//! metric's own keys are read from `infra/logging.tf` and checked against
//! it.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use kiewer_assert::assert_is_empty;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::beacon::{BEACON_PATH, LIMIT_PER_MINUTE, MAX_BODY, MAX_REASON, MAX_VENDOR};
use kiewer_lib::web::viewer::page_script_url;
use kiewer_test_support::db::fresh;
use kiewer_test_support::{TempDir, http, scratch_dir};
use test_case::test_case;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;

const CHROME: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// The beacon's log lines as JSON. A global subscriber is per process,
/// and every module of this test binary runs under this one once a test
/// here installs it, so it keeps only the beacon handler's target: the
/// rest of the binary's events would grow the buffer for the whole run.
/// Each test finds its own lines by a marker no other test uses. It is
/// the binary's only global subscriber: installing a second one fails, so
/// another module that needs a log scopes its own with
/// `tracing::subscriber::with_default`.
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
        let subscriber = tracing_subscriber::registry().with(
            tracing_stackdriver::layer()
                .with_writer(move || LogWriter(Arc::clone(&writer)))
                .with_filter(Targets::new().with_target("kiewer_lib::web::beacon", Level::TRACE)),
        );
        tracing::subscriber::set_global_default(subscriber).expect("the one global subscriber");
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
    let state = http::state(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
    );
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
    assert_eq!(line["severity"], "INFO");
    assert_eq!(line["target"], "kiewer_lib::web::beacon");
    assert_eq!(line["message"], "renderer beacon");
    assert_eq!(line["backend"], "webgpu");
    assert_eq!(line["webgpuPresent"], true);
    assert_eq!(line["outcome"], "ok");
    assert_eq!(line["reason"], "");
    assert_eq!(line["vendor"], vendor);
    assert_eq!(line["userAgentFamily"], "chrome");
    assert!(
        line.get("ip").is_none() && !line.to_string().contains("203.0.113.10"),
        "no identifier in the line: {line}"
    );
}

/// The log-based metric the line feeds.
const METRIC: &str = include_str!("../../../infra/logging.tf");

/// Each of the metric's labels with the `jsonPayload` key its extractor
/// reads (the `label = "EXTRACT(jsonPayload.key)"` lines), sorted by
/// label.
fn extractors() -> Vec<(&'static str, &'static str)> {
    let mut pairs: Vec<(&str, &str)> = METRIC
        .lines()
        .filter_map(|line| {
            let (label, rest) = line.split_once("= \"EXTRACT(jsonPayload.")?;
            let key = rest
                .trim_end()
                .strip_suffix(")\"")
                .expect("the extractor closes");
            Some((label.trim(), key))
        })
        .collect();
    pairs.sort_unstable();
    pairs
}

/// The key and value of each `jsonPayload` term in the metric's filter.
fn filtered_fields() -> Vec<(&'static str, &'static str)> {
    METRIC
        .lines()
        .filter_map(|line| line.trim().strip_prefix("jsonPayload."))
        .map(|term| {
            let (key, value) = term.split_once('=').expect("a key and a value");
            (key, value.trim_matches('"'))
        })
        .collect()
}

#[tokio::test]
async fn the_metric_reads_the_keys_the_line_is_written_with() {
    let (addr, _dir) = start().await;
    let vendor = fresh("vendor");
    let response = send(addr, "203.0.113.12", &beacon(&vendor)).await;
    assert_eq!(response.status, 204, "{}", response.body);
    let lines = log_lines_with(&vendor);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    // What each label must read from the beacon sent above, named by the
    // label: a misnamed extractor reads nothing, and two swapped ones read
    // each other's value, since the four values differ.
    let expected = [
        ("backend", serde_json::json!("webgpu")),
        ("outcome", serde_json::json!("ok")),
        ("user_agent_family", serde_json::json!("chrome")),
        ("webgpu_present", serde_json::json!(true)),
    ];
    let extractors = extractors();
    let labels: Vec<&str> = extractors.iter().map(|(label, _)| *label).collect();
    let expected_labels: Vec<&str> = expected.iter().map(|(label, _)| *label).collect();
    assert_eq!(labels, expected_labels);
    for ((label, key), (_, value)) in extractors.iter().zip(&expected) {
        assert_eq!(
            &line[*key], value,
            "the {label} label reads jsonPayload.{key}: {line}"
        );
    }
    let filter = filtered_fields();
    assert_eq!(filter.len(), 2, "{filter:?}");
    for (key, value) in filter {
        assert_eq!(line[key], value, "the filter on {key}: {line}");
    }
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
    assert_eq!(lines[0]["webgpuPresent"], false);
    assert_eq!(lines[0]["outcome"], "failed");
    assert_eq!(lines[0]["reason"], reason);
    assert_eq!(lines[0]["vendor"], "");
    assert_eq!(lines[0]["userAgentFamily"], "safari");

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
    assert_eq!(lines[0]["userAgentFamily"], "other", "no user agent at all");
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

#[test_case("reason", MAX_REASON + 1, 400 ; "a reason over its bound")]
#[test_case("vendor", MAX_VENDOR + 1, 400 ; "a vendor over its bound")]
#[test_case("reason", MAX_REASON, 204 ; "a reason at its bound")]
#[test_case("vendor", MAX_VENDOR, 204 ; "a vendor at its bound")]
#[tokio::test]
async fn a_string_is_accepted_at_its_bound_and_refused_over_it(
    field: &str,
    length: usize,
    status: u16,
) {
    let (addr, _dir) = start().await;
    let marker = fresh("marker");
    let value = format!("{marker}{}", "x".repeat(length - marker.len()));
    let body = format!(
        r#"{{"backend":"webgpu","webgpu_present":true,"outcome":"panic","{field}":"{value}"}}"#
    );
    let response = send(addr, "203.0.113.21", &body).await;
    assert_eq!(response.status, status, "{}", response.body);
    assert_eq!(
        log_lines_with(&marker).len(),
        usize::from(status == 204),
        "logged only when accepted"
    );
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let (addr, _dir) = start().await;
    let marker = fresh("marker");
    let body = format!(r#"{{"reason":"{marker}{}"#, "x".repeat(MAX_BODY));
    let response = send(addr, "203.0.113.22", &body).await;
    assert_eq!(response.status, 413, "{}", response.body);
    assert_is_empty!(log_lines_with(&marker));
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

#[test_case("/viewer"; "the board page")]
#[test_case("/schematic"; "the schematic page")]
#[tokio::test]
async fn a_page_sends_the_beacon_and_keeps_a_panic_for_it_through_the_page_script(page: &str) {
    let (addr, _dir) = start().await;
    let (status, html) = http::get(addr, page).await;
    assert_eq!(status, 200);
    assert!(
        html.contains(&format!("from \"{}\"", page_script_url())),
        "{page} imports the page script: {html}"
    );
    let (status, body) = http::get(addr, &page_script_url()).await;
    assert_eq!(status, 200);
    assert!(body.contains("navigator.sendBeacon"), "{body}");
    assert!(body.contains(BEACON_PATH), "{body}");
    assert!(
        body.contains("\"gpu\" in navigator"),
        "the page says whether WebGPU existed: {body}"
    );
    assert!(body.contains("adapter_vendor()"), "{body}");
    let hook = body
        .find("panicked at")
        .expect("the panic hook reads the hook's message");
    let entry = body
        .find("export async function start")
        .expect("the entry a page calls");
    assert!(
        hook < entry,
        "the panic hook is set when the module is evaluated, before a page can start it, \
         so a panic in init is kept"
    );
    assert!(body.contains("kiewer-panic"), "{body}");
}
