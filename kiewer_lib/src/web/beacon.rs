//! The renderer beacon (docs/v1-scope.md, Telemetry): the one report the
//! viewer page sends per page load through `navigator.sendBeacon`, with
//! which backend ran, whether the browser exposed WebGPU at all, how the
//! start went and which adapter vendor the browser named; a wasm panic
//! sends the same report with its message, and a canvas held to the
//! device's texture limit sends one more, once. The server checks the shape,
//! limits each client to [`LIMIT_PER_MINUTE`] and writes one `tracing`
//! event with the fields below, which the JSON log the server runs with
//! on Cloud Run makes one `jsonPayload` line and a Cloud Logging metric
//! counts (`infra/logging.tf`). That log camelCases a field's key as it
//! writes the line (`webgpu_present` is `webgpuPresent`), so renaming a
//! field here renames the key the metric extracts, and
//! `kiewer_lib/tests/it/beacon.rs` fails until both agree. A refused beacon
//! is a 4xx and no event. No identifier is read or written: the client's
//! address keys the limit and goes nowhere else. The limiter is per
//! process: each Cloud Run instance counts on its own, so a client's
//! ceiling is the limit times the instances serving it, which telemetry
//! can bear.
//!
//! The body is read as bytes and parsed as JSON whatever its content
//! type: `sendBeacon` sends a string as `text/plain`, and a `Blob` typed
//! `application/json` would cost a CORS preflight on any other origin.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{RETRY_AFTER, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use derive_more::Display;
use serde::Deserialize;

use super::AppState;
use crate::cache::Policy;
use crate::error::{Error, Result};

/// Where the page posts to.
pub const BEACON_PATH: &str = "/api/beacon";

/// The largest body read, in bytes: a beacon is a few hundred, with the
/// reason's [`MAX_REASON`] characters on top. Over it is a 413.
pub const MAX_BODY: usize = 4096;

/// The longest `reason`, in characters: a panic message with its
/// location, or the fallback reasons joined. The page truncates to it.
pub const MAX_REASON: usize = 1024;

/// The longest `vendor`, in characters: a vendor name, never a model.
pub const MAX_VENDOR: usize = 64;

/// Beacons one client may send per minute. A page load sends one and a
/// panic a second; a person reloading, or an office behind one address,
/// stays far under it, a loop does not.
pub const LIMIT_PER_MINUTE: u32 = 30;

/// Distinct clients the limiter tracks within one window. Past it a new
/// client is refused until the window turns: a flood from that many
/// addresses in a minute is noise, and losing its telemetry costs less
/// than the map growing with it.
pub const MAX_CLIENTS: usize = 10_000;

const WINDOW: Duration = Duration::from_secs(60);

/// The beacon's routes with their own limiter, merged into the server's
/// router. One limiter per router, so one per process.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(BEACON_PATH, post(receive))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(Arc::new(Limiter::new(LIMIT_PER_MINUTE, WINDOW)))
}

/// Which backend drew, or `none` when neither could be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Display)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[display("webgpu")]
    WebGpu,
    #[display("webgl2")]
    WebGl2,
    #[display("none")]
    None,
}

/// How the page load went: the adapter and device came up, they did
/// not, or the wasm panicked (at any point in the page's life); or the
/// canvas was held to the device's texture limit, sent beside `ok` the
/// first time it happens in a page load (`cap_canvas` in
/// `kiewer_viewer/src/render.rs`), with no reason and no sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Display)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    #[display("ok")]
    Ok,
    #[display("failed")]
    Failed,
    #[display("panic")]
    Panic,
    #[display("capped")]
    Capped,
}

/// One report as the page sends it. Every field is in the closed sets or
/// under the bounds above; anything else, or any field not named here,
/// is malformed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Beacon {
    pub backend: Backend,
    /// Whether `navigator.gpu` existed, whatever came of it.
    pub webgpu_present: bool,
    pub outcome: Outcome,
    /// Why it failed or what the panic said; empty on success.
    #[serde(default)]
    pub reason: String,
    /// The adapter vendor the browser named, lowercased; empty when the
    /// browser exposes none.
    #[serde(default)]
    pub vendor: String,
}

impl Beacon {
    /// The beacon in `body`, or [`Error::Malformed`] naming what is wrong
    /// with it.
    ///
    /// # Errors
    ///
    /// When the body is not a JSON object of exactly these fields, or a
    /// string in it is over its bound.
    pub fn parse(body: &[u8]) -> Result<Beacon> {
        let beacon: Beacon =
            serde_json::from_slice(body).map_err(|error| Error::Malformed(error.to_string()))?;
        let reason = beacon.reason.chars().count();
        if reason > MAX_REASON {
            return Err(Error::Malformed(format!(
                "reason is {reason} characters, over {MAX_REASON}"
            )));
        }
        let vendor = beacon.vendor.chars().count();
        if vendor > MAX_VENDOR {
            return Err(Error::Malformed(format!(
                "vendor is {vendor} characters, over {MAX_VENDOR}"
            )));
        }
        Ok(beacon)
    }
}

/// The browser family a `User-Agent` names, as the log line carries it.
/// A Chromium derivative that says `Chrome/` (Brave, Opera, Vivaldi) is
/// Chrome: the renderer is what the beacon is about, and that is the
/// engine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display)]
pub enum UserAgentFamily {
    #[display("chrome")]
    Chrome,
    #[display("firefox")]
    Firefox,
    #[display("safari")]
    Safari,
    #[display("edge")]
    Edge,
    #[display("other")]
    Other,
}

impl UserAgentFamily {
    /// The family of `user_agent`. Edge before Chrome (Edge says both),
    /// Chrome before Safari (Chrome says both), and Firefox on iOS says
    /// `FxiOS/` as Chrome there says `CriOS/`.
    #[must_use]
    pub fn of(user_agent: &str) -> UserAgentFamily {
        if user_agent.contains("Edg/") || user_agent.contains("EdgiOS/") {
            UserAgentFamily::Edge
        } else if user_agent.contains("Chrome/") || user_agent.contains("CriOS/") {
            UserAgentFamily::Chrome
        } else if user_agent.contains("Firefox/") || user_agent.contains("FxiOS/") {
            UserAgentFamily::Firefox
        } else if user_agent.contains("Safari/") {
            UserAgentFamily::Safari
        } else {
            UserAgentFamily::Other
        }
    }
}

/// Who a beacon is from, for the limit alone: an address, or unknown
/// when no proxy named one (a dev box). Never logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Client {
    Ip(IpAddr),
    Unknown,
}

impl Client {
    /// The client behind `headers`. `Fastly-Client-IP` is the one trusted
    /// name: Fastly fronts the hostname (ADR 0003) and sets it from the
    /// connection on the first edge visit (`infra/fastly/recv.vcl.tftpl`).
    /// Without it the request came straight to the Cloud Run URL, where
    /// the last `X-Forwarded-For` entry is the one Cloud Run's front end
    /// appended, the connecting peer: advisory, since that peer may be a
    /// proxy of the client's own, but not the client's to forge.
    #[must_use]
    pub fn of(headers: &HeaderMap) -> Client {
        let fastly = headers
            .get("fastly-client-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());
        if let Some(ip) = fastly {
            return Client::Ip(ip);
        }
        headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(|value| value.trim().parse().ok())
            .map_or(Client::Unknown, Client::Ip)
    }
}

/// A fixed window per client: at most `limit` beacons from one client
/// within `window` of the window's start, counted in a map the next
/// window begins empty. The map holds at most [`MAX_CLIENTS`] clients.
#[derive(Debug)]
pub struct Limiter {
    limit: u32,
    window: Duration,
    state: Mutex<Counts>,
}

#[derive(Debug)]
struct Counts {
    started: Instant,
    per_client: HashMap<Client, u32>,
}

impl Limiter {
    /// A limiter of `limit` per `window`, with its first window starting
    /// now.
    #[must_use]
    pub fn new(limit: u32, window: Duration) -> Limiter {
        Limiter {
            limit,
            window,
            state: Mutex::new(Counts {
                started: Instant::now(),
                per_client: HashMap::new(),
            }),
        }
    }

    /// Whether `client` may send one more at `now`; counted when it may.
    /// `now` is a parameter so a test can turn the window.
    pub fn allows(&self, client: Client, now: Instant) -> bool {
        // A poisoned lock holds a map of counters, nothing an unwinding
        // thread can leave half-written in a way that matters here.
        let mut counts = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(counts.started) >= self.window {
            counts.started = now;
            counts.per_client.clear();
        }
        if !counts.per_client.contains_key(&client) && counts.per_client.len() >= MAX_CLIENTS {
            return false;
        }
        let count = counts.per_client.entry(client).or_insert(0);
        if *count >= self.limit {
            return false;
        }
        *count += 1;
        true
    }

    /// How many clients the current window has seen.
    #[must_use]
    pub fn clients(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_client
            .len()
    }
}

async fn receive(State(limiter): State<Arc<Limiter>>, headers: HeaderMap, body: Bytes) -> Response {
    if !limiter.allows(Client::of(&headers), Instant::now()) {
        let retry_after = WINDOW.as_secs().to_string();
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                Policy::Personal.header(),
                (RETRY_AFTER, retry_after.as_str()),
            ],
            "too many beacons\n",
        )
            .into_response();
    }
    let Ok(beacon) = Beacon::parse(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            [Policy::Personal.header()],
            "malformed beacon\n",
        )
            .into_response();
    };
    let family = headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map_or(UserAgentFamily::Other, UserAgentFamily::of);
    tracing::info!(
        backend = %beacon.backend,
        webgpu_present = beacon.webgpu_present,
        outcome = %beacon.outcome,
        reason = %beacon.reason,
        vendor = %beacon.vendor,
        user_agent_family = %family,
        "renderer beacon"
    );
    (StatusCode::NO_CONTENT, [Policy::Personal.header()]).into_response()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36", UserAgentFamily::Chrome ; "chrome on linux")]
    #[test_case("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0", UserAgentFamily::Edge ; "edge on windows")]
    #[test_case("Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0", UserAgentFamily::Firefox ; "firefox on linux")]
    #[test_case("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15", UserAgentFamily::Safari ; "safari on a mac")]
    #[test_case("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/130.0.0.0 Mobile/15E148 Safari/604.1", UserAgentFamily::Chrome ; "chrome on an iphone")]
    #[test_case("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/132.0 Mobile/15E148 Safari/605.1.15", UserAgentFamily::Firefox ; "firefox on an iphone")]
    #[test_case("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 OPR/115.0.0.0", UserAgentFamily::Chrome ; "opera is chromium")]
    #[test_case("curl/8.5.0", UserAgentFamily::Other ; "curl")]
    #[test_case("", UserAgentFamily::Other ; "empty")]
    fn the_family_of_a_user_agent(user_agent: &str, family: UserAgentFamily) {
        assert_eq!(UserAgentFamily::of(user_agent), family);
    }

    #[test_case(Backend::WebGpu, "webgpu")]
    #[test_case(Backend::WebGl2, "webgl2")]
    #[test_case(Backend::None, "none")]
    fn a_backend_has_one_wire_name(backend: Backend, name: &str) {
        assert_eq!(backend.to_string(), name, "as the log line writes it");
        let parsed: Backend = serde_json::from_str(&format!("\"{name}\"")).expect("parses");
        assert_eq!(parsed, backend, "as the page sends it");
    }

    #[test_case(Outcome::Ok, "ok")]
    #[test_case(Outcome::Failed, "failed")]
    #[test_case(Outcome::Panic, "panic")]
    #[test_case(Outcome::Capped, "capped")]
    fn an_outcome_has_one_wire_name(outcome: Outcome, name: &str) {
        assert_eq!(outcome.to_string(), name, "as the log line writes it");
        let parsed: Outcome = serde_json::from_str(&format!("\"{name}\"")).expect("parses");
        assert_eq!(parsed, outcome, "as the page sends it");
    }

    fn ip(text: &str) -> Client {
        Client::Ip(text.parse().expect("an address literal"))
    }

    #[test]
    fn a_client_is_counted_up_to_the_limit_within_a_window() {
        let limiter = Limiter::new(3, Duration::from_secs(60));
        let start = Instant::now();
        let a = ip("203.0.113.1");
        let b = ip("203.0.113.2");
        assert!(limiter.allows(a, start));
        assert!(limiter.allows(a, start + Duration::from_secs(10)));
        assert!(limiter.allows(a, start + Duration::from_secs(20)));
        assert!(!limiter.allows(a, start + Duration::from_secs(30)));
        assert!(
            limiter.allows(b, start + Duration::from_secs(30)),
            "another client"
        );
        assert_eq!(limiter.clients(), 2);
    }

    #[test]
    fn the_next_window_starts_empty() {
        let limiter = Limiter::new(1, Duration::from_secs(60));
        let start = Instant::now();
        let a = ip("203.0.113.1");
        assert!(limiter.allows(a, start));
        assert!(!limiter.allows(a, start + Duration::from_secs(59)));
        assert!(
            limiter.allows(a, start + Duration::from_secs(60)),
            "a new window"
        );
        assert_eq!(limiter.clients(), 1, "the old window's clients are gone");
        assert!(!limiter.allows(a, start + Duration::from_secs(61)));
    }

    #[test]
    fn the_map_stops_at_max_clients_until_the_window_turns() {
        let limiter = Limiter::new(1, Duration::from_secs(60));
        let start = Instant::now();
        for n in 0..MAX_CLIENTS {
            let address = u32::try_from(n).expect("MAX_CLIENTS fits an IPv4 address");
            let client = Client::Ip(IpAddr::from(std::net::Ipv4Addr::from_bits(address)));
            assert!(limiter.allows(client, start));
        }
        assert_eq!(limiter.clients(), MAX_CLIENTS);
        let late = ip("203.0.113.9");
        assert!(!limiter.allows(late, start), "a new client past the cap");
        assert_eq!(limiter.clients(), MAX_CLIENTS, "and it is not kept");
        assert!(limiter.allows(late, start + Duration::from_secs(60)));
        assert_eq!(limiter.clients(), 1);
    }

    #[test]
    fn the_unknown_client_is_one_client() {
        let limiter = Limiter::new(1, Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.allows(Client::Unknown, start));
        assert!(!limiter.allows(Client::Unknown, start));
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                value.parse().expect("a header value"),
            );
        }
        headers
    }

    #[test]
    fn fastly_names_the_client_and_wins_over_forwarded_for() {
        let found = Client::of(&headers(&[
            ("X-Forwarded-For", "198.51.100.1, 203.0.113.5"),
            ("Fastly-Client-IP", "2001:db8::7"),
        ]));
        assert_eq!(found, ip("2001:db8::7"));
    }

    #[test]
    fn without_fastly_the_last_forwarded_for_entry_of_the_last_header_is_the_client() {
        let found = Client::of(&headers(&[
            ("X-Forwarded-For", "198.51.100.1"),
            ("X-Forwarded-For", "198.51.100.2, 203.0.113.5"),
        ]));
        assert_eq!(found, ip("203.0.113.5"));
    }

    #[test]
    fn no_header_or_no_address_is_unknown() {
        assert_eq!(Client::of(&headers(&[])), Client::Unknown);
        assert_eq!(
            Client::of(&headers(&[("X-Forwarded-For", "not an address")])),
            Client::Unknown
        );
        assert_eq!(
            Client::of(&headers(&[("Fastly-Client-IP", "unknown")])),
            Client::Unknown
        );
    }

    #[test]
    fn a_beacon_parses_with_its_optional_strings_defaulting_to_empty() {
        let beacon =
            Beacon::parse(br#"{"backend":"webgl2","webgpu_present":false,"outcome":"ok"}"#)
                .expect("a minimal beacon");
        assert_eq!(
            beacon,
            Beacon {
                backend: Backend::WebGl2,
                webgpu_present: false,
                outcome: Outcome::Ok,
                reason: String::new(),
                vendor: String::new(),
            }
        );
    }

    #[test]
    fn a_bound_counts_characters_not_bytes() {
        let reason = "é".repeat(MAX_REASON);
        let body = format!(
            r#"{{"backend":"webgpu","webgpu_present":true,"outcome":"panic","reason":"{reason}"}}"#
        );
        assert!(body.len() > MAX_REASON + 100, "two bytes a character");
        assert!(Beacon::parse(body.as_bytes()).is_ok());
    }
}
