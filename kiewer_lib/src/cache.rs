//! The caching policy (ADR 0003; docs/v1-scope.md, Caching). Every
//! response names a [`Policy`]: a page the same for everyone, which
//! Fastly keeps until a write purges it; a personal response a shared
//! cache never keeps; an immutable asset or blob kept for a year; a 404
//! kept briefly. A cacheable page carries [`Key`]s in `Surrogate-Key`,
//! and the writes that change it purge those keys through the
//! [`Purge`] client: [`Fastly`] in production, [`NoPurge`] on a dev box,
//! [`Recorder`] in a test. The rule for a handler: every response names
//! its policy, and a page under a design or a handle carries their keys.
//! What each write purges is [`Key`]'s documentation.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::header::{CACHE_CONTROL, HeaderName};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};

use crate::account::Handle;
use crate::design::DesignId;
use crate::error::{Error, Result};

/// The header Fastly reads the keys from and strips before the client.
pub const SURROGATE_KEY: HeaderName = HeaderName::from_static("surrogate-key");

/// How a response may be cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Policy {
    /// A page or a static text the same for everyone: Fastly keeps it an
    /// hour, or until a write purges its keys; the browser asks Fastly
    /// again on every visit (`max-age=0`), so a purge reaches the next
    /// view. The hour bounds a page nothing purges (the about page, the
    /// hand-off viewer) after a deploy, and a purge that was lost.
    Page,
    /// A response that depends on the session or the browser: a settings
    /// or upload page, `/api/me`, a redirect after a write. No cache
    /// keeps it.
    Personal,
    /// Bytes addressed by their content: a blob, a hashed asset. A year,
    /// and `immutable` so the browser never revalidates.
    Immutable,
    /// A 404 for a design, a person or a blob that may exist soon: kept a
    /// minute at Fastly, so a new design is reachable within one.
    NotFound,
}

impl Policy {
    /// The `Cache-Control` value.
    #[must_use]
    pub fn value(self) -> &'static str {
        match self {
            Policy::Page => "public, max-age=0, s-maxage=3600, stale-while-revalidate=60",
            Policy::Personal => "private, no-store",
            Policy::Immutable => "public, max-age=31536000, immutable",
            Policy::NotFound => "public, max-age=0, s-maxage=60",
        }
    }

    /// The header pair, for a response's header list.
    #[must_use]
    pub fn header(self) -> (HeaderName, &'static str) {
        (CACHE_CONTROL, self.value())
    }
}

/// What a cacheable page is about, as Fastly indexes it: a purge of a
/// key drops every page that carried it. A page carries the key of each
/// thing it shows, and a write purges the keys of what it changed:
///
/// - a push: the design (its pages, their dropdowns and banners), the
///   owner's handle (the person page's listing) and the home page;
/// - a visibility change: the same three;
/// - a transfer: the design, both handles and the home page;
/// - the settings form (name, slug, description): the design, the
///   owner's handle and the home page;
/// - a role change: nothing, since no cacheable page shows roles.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// The home page's recent list.
    Home,
    /// Every page of one design: the design, branch, revision, history
    /// and schematic pages, and the redirect from a slug it left.
    Design(DesignId),
    /// A person's page, and whatever else lists their designs by handle.
    Handle(Handle),
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Home => f.write_str("home"),
            Key::Design(id) => write!(f, "design-{id}"),
            Key::Handle(handle) => write!(f, "handle-{handle}"),
        }
    }
}

/// The `Surrogate-Key` value for `keys`: space separated, as Fastly reads
/// them.
#[must_use]
pub fn surrogate_key(keys: &[Key]) -> String {
    keys.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `response` as a cacheable page about `keys`: [`Policy::Page`] and the
/// `Surrogate-Key` header. The page itself must be the same for everyone
/// (ADR 0003): nothing a handler puts in it may come from the session.
pub fn page(keys: &[Key], response: impl IntoResponse) -> Response {
    let mut response = ([Policy::Page.header()], response).into_response();
    if !keys.is_empty()
        && let Ok(value) = HeaderValue::from_str(&surrogate_key(keys))
    {
        response.headers_mut().insert(SURROGATE_KEY, value);
    }
    response
}

/// What purges keys at the edge.
#[async_trait]
pub trait Purge: Send + Sync + fmt::Debug {
    /// Drop every cached page carrying any of `keys`.
    ///
    /// # Errors
    ///
    /// When the edge refused or could not be reached; the write the purge
    /// followed stands, and [`after_write`] logs it.
    async fn purge(&self, keys: &[Key]) -> Result<()>;
}

/// The purge that follows a write: the keys go out, and a failure is a
/// warning in the log, never the write's failure, since the rows are
/// written and the page is at most an hour stale ([`Policy::Page`]).
pub async fn after_write(purge: &dyn Purge, keys: &[Key]) {
    if let Err(error) = purge.purge(keys).await {
        tracing::warn!(%error, keys = %surrogate_key(keys), "the purge failed");
    }
}

/// No edge in front of the server: a dev box, or a deployment without a
/// Fastly token yet. Purges nothing and says so at debug.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPurge;

#[async_trait]
impl Purge for NoPurge {
    async fn purge(&self, keys: &[Key]) -> Result<()> {
        tracing::debug!(keys = %surrogate_key(keys), "no edge to purge");
        Ok(())
    }
}

/// A purge that keeps every call, for a test to read back what a write
/// purged.
#[derive(Debug, Default)]
pub struct Recorder {
    calls: Mutex<Vec<Vec<Key>>>,
}

impl Recorder {
    /// Every purge so far, in order, each the keys of one call.
    ///
    /// # Panics
    ///
    /// Never: the lock holds a list, nothing an unwinding thread can
    /// leave half-written in a way that matters.
    #[must_use]
    pub fn calls(&self) -> Vec<Vec<Key>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every key purged so far, in order, with repeats.
    #[must_use]
    pub fn keys(&self) -> Vec<Key> {
        self.calls().into_iter().flatten().collect()
    }

    /// Forget what was recorded, so a test reads one write's purges.
    pub fn clear(&self) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[async_trait]
impl Purge for Recorder {
    async fn purge(&self, keys: &[Key]) -> Result<()> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(keys.to_vec());
        Ok(())
    }
}

/// Where Fastly's API is.
pub const FASTLY_API: &str = "https://api.fastly.com";

/// How long one purge call may take: the edge answers in well under a
/// second, and a write's response waits on it.
const PURGE_TIMEOUT: Duration = Duration::from_secs(5);

/// The purge-by-key call of Fastly's API: `POST
/// /service/{service id}/purge` with the token in `Fastly-Key` and the
/// keys in a `Surrogate-Key` request header, which drops them across
/// every POP at once. At most 256 keys go in one call; a write has four
/// at most.
#[derive(Clone)]
pub struct Fastly {
    service_id: String,
    token: String,
    api: String,
    http: reqwest::Client,
}

impl fmt::Debug for Fastly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fastly")
            .field("service_id", &self.service_id)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl Fastly {
    /// The client for `service_id` with `token`, against [`FASTLY_API`].
    ///
    /// # Errors
    ///
    /// [`Error::Http`] when the HTTP client cannot be built.
    pub fn new(service_id: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Self::at(FASTLY_API, service_id, token)
    }

    /// The client against another API origin: a test's fake.
    ///
    /// # Errors
    ///
    /// [`Error::Http`] when the HTTP client cannot be built.
    pub fn at(
        api: impl Into<String>,
        service_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(PURGE_TIMEOUT)
            .user_agent(concat!("kiewer/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            service_id: service_id.into(),
            token: token.into(),
            api: api.into().trim_end_matches('/').to_owned(),
            http,
        })
    }

    /// The service purged on.
    #[must_use]
    pub fn service_id(&self) -> &str {
        &self.service_id
    }
}

#[async_trait]
impl Purge for Fastly {
    async fn purge(&self, keys: &[Key]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let url = format!("{}/service/{}/purge", self.api, self.service_id);
        let response = self
            .http
            .post(&url)
            .header("Fastly-Key", &self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(SURROGATE_KEY, surrogate_key(keys))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Failed {
                reason: format!(
                    "Fastly answered {status} to the purge of {}",
                    surrogate_key(keys)
                ),
            });
        }
        tracing::info!(keys = %surrogate_key(keys), "purged");
        Ok(())
    }
}

/// A content coding the server has an asset in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

impl Encoding {
    /// The `Content-Encoding` value, or `None` for identity, which sends
    /// no header.
    #[must_use]
    pub fn content_encoding(self) -> Option<&'static str> {
        match self {
            Encoding::Brotli => Some("br"),
            Encoding::Gzip => Some("gzip"),
            Encoding::Identity => None,
        }
    }

    /// The coding to send from what `Accept-Encoding` takes: brotli when
    /// it is accepted, else gzip, else identity. A coding with `q=0` is
    /// refused; `*` accepts any; no header accepts identity alone, as
    /// curl sends.
    #[must_use]
    pub fn negotiate(headers: &HeaderMap) -> Encoding {
        let mut brotli = false;
        let mut gzip = false;
        for value in headers.get_all(axum::http::header::ACCEPT_ENCODING) {
            let Ok(value) = value.to_str() else {
                continue;
            };
            for coding in value.split(',') {
                let coding = coding.trim();
                let name = coding.split(';').next().unwrap_or(coding).trim();
                let refused = coding.split(';').skip(1).any(|param| {
                    param
                        .trim()
                        .strip_prefix("q=")
                        .and_then(|q| q.trim().parse::<f32>().ok())
                        .is_some_and(|q| q <= 0.0)
                });
                if refused {
                    continue;
                }
                match name {
                    "br" => brotli = true,
                    "gzip" => gzip = true,
                    "*" => {
                        brotli = true;
                        gzip = true;
                    }
                    _ => {}
                }
            }
        }
        if brotli {
            Encoding::Brotli
        } else if gzip {
            Encoding::Gzip
        } else {
            Encoding::Identity
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn handle(text: &str) -> Handle {
        text.parse().expect("a handle")
    }

    #[test]
    fn a_key_names_what_it_is_about_without_spaces() {
        assert_eq!(Key::Home.to_string(), "home");
        assert_eq!(Key::Design(DesignId::for_test(42)).to_string(), "design-42");
        assert_eq!(Key::Handle(handle("ada-l")).to_string(), "handle-ada-l");
        assert_eq!(
            surrogate_key(&[
                Key::Design(DesignId::for_test(7)),
                Key::Handle(handle("ada")),
                Key::Home
            ]),
            "design-7 handle-ada home"
        );
    }

    #[test_case(
        Policy::Page,
        "public, max-age=0, s-maxage=3600, stale-while-revalidate=60"
    )]
    #[test_case(Policy::Personal, "private, no-store")]
    #[test_case(Policy::Immutable, "public, max-age=31536000, immutable")]
    #[test_case(Policy::NotFound, "public, max-age=0, s-maxage=60")]
    fn each_policy_is_one_cache_control_value(policy: Policy, value: &str) {
        assert_eq!(policy.value(), value);
        assert_eq!(policy.header(), (CACHE_CONTROL, value));
    }

    #[test]
    fn a_page_carries_its_policy_and_its_keys() {
        let response = page(&[Key::Home, Key::Handle(handle("ada"))], "hello");
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            Policy::Page.value()
        );
        assert_eq!(
            response.headers().get(SURROGATE_KEY).unwrap(),
            "home handle-ada"
        );
        let keyless = page(&[], "hello");
        assert!(
            keyless.headers().get(SURROGATE_KEY).is_none(),
            "a page about nothing purgeable carries no header"
        );
    }

    #[tokio::test]
    async fn the_recorder_keeps_each_call_and_the_no_purge_keeps_nothing() {
        let recorder = Recorder::default();
        after_write(&recorder, &[Key::Home]).await;
        after_write(&recorder, &[Key::Design(DesignId::for_test(1)), Key::Home]).await;
        assert_eq!(
            recorder.calls(),
            [
                vec![Key::Home],
                vec![Key::Design(DesignId::for_test(1)), Key::Home]
            ]
        );
        assert_eq!(
            recorder.keys(),
            [Key::Home, Key::Design(DesignId::for_test(1)), Key::Home]
        );
        recorder.clear();
        assert!(recorder.calls().is_empty());
        NoPurge.purge(&[Key::Home]).await.expect("nothing to fail");
    }

    fn accept(value: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = value {
            headers.insert(
                axum::http::header::ACCEPT_ENCODING,
                value.parse().expect("a header value"),
            );
        }
        headers
    }

    #[test_case(Some("gzip, deflate, br"), Encoding::Brotli; "a browser")]
    #[test_case(Some("gzip, deflate, br, zstd"), Encoding::Brotli; "a newer browser")]
    #[test_case(Some("gzip;q=1.0, identity; q=0.5"), Encoding::Gzip; "gzip weighted")]
    #[test_case(Some("br;q=0, gzip"), Encoding::Gzip; "brotli refused")]
    #[test_case(Some("br;q=0.0, gzip"), Encoding::Gzip; "brotli refused at length")]
    #[test_case(Some("br;q=0, gzip;q=0"), Encoding::Identity; "both refused")]
    #[test_case(Some("*"), Encoding::Brotli; "anything")]
    #[test_case(Some("identity"), Encoding::Identity; "identity only")]
    #[test_case(Some("deflate"), Encoding::Identity; "a coding the server lacks")]
    #[test_case(None, Encoding::Identity; "no header, as curl sends")]
    fn the_best_coding_the_client_takes_is_sent(value: Option<&str>, encoding: Encoding) {
        assert_eq!(Encoding::negotiate(&accept(value)), encoding);
    }

    #[test]
    fn identity_sends_no_content_encoding() {
        assert_eq!(Encoding::Brotli.content_encoding(), Some("br"));
        assert_eq!(Encoding::Gzip.content_encoding(), Some("gzip"));
        assert_eq!(Encoding::Identity.content_encoding(), None);
    }

    #[test]
    fn the_fastly_client_names_its_service_and_hides_its_token() {
        let fastly = Fastly::new("SU1Z0isxPaozGVKXdv0eY", "secret").expect("a client");
        assert_eq!(fastly.service_id(), "SU1Z0isxPaozGVKXdv0eY");
        let debug = format!("{fastly:?}");
        assert!(debug.contains("SU1Z0isxPaozGVKXdv0eY"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
