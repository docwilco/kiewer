//! The caching policy (ADR 0003; docs/v1-scope.md, Caching). Every
//! response names a [`Policy`]: a page the same for everyone, which
//! Fastly keeps until a write purges it; a personal response a shared
//! cache never keeps; an immutable asset or blob kept for a year; a 404
//! kept briefly. A cacheable page carries [`Key`]s in `Surrogate-Key`,
//! and the writes that change it purge those keys through the
//! [`Purge`] client: [`Fastly`] in production, [`NoPurge`] on a dev box,
//! [`Recorder`] in a test. The rule for a handler: every response it
//! builds names its policy, and a page under a design or a handle
//! carries their keys. What axum answers by itself (a 405, an extractor's
//! rejection) names none, and Fastly's `default_ttl = 0` keeps it out of
//! the cache.
//! What each write purges is [`Key`]'s documentation. On kiewer-staging
//! every key, in a header and in a purge, carries a [`KeyPrefix`].

use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::header::{ACCEPT_ENCODING, CACHE_CONTROL, HeaderName};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::account::Handle;
use crate::design::DesignId;
use crate::error::{Error, Result};
use crate::roles::Visibility;

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
    /// A response no cache may keep: one that depends on the session or
    /// the browser (a settings or upload page, `/api/me`, a redirect
    /// after a write), and one that must not outlive the moment (a 500, a
    /// thumbnail not rendered yet).
    Personal,
    /// Bytes addressed by their content: a blob, a hashed asset. A year,
    /// and `immutable` so the browser never revalidates.
    Immutable,
    /// A 404 for a design, a person or a blob that may exist soon, and
    /// the answer for a blocked blob, which a cleared takedown must be
    /// able to undo: kept a minute at Fastly, never immutable. A 404 for
    /// a person or a design carries the handle's key
    /// ([`not_found_under`]), so the write that makes it exist clears it
    /// at once.
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
/// - a push, a fork, the settings form (name, slug, description), a
///   change to a branch (new, renamed, deleted, made the default) or the
///   decision on a pending revision: [`of_design`], the design, the owner's
///   handle, and the home page when the design is public; a fork's
///   source's pages do not change; a pending revision purges nothing until
///   it is decided, since no page shows it;
/// - a visibility change: the design, the owner's handle and the home
///   page, whichever way it went;
/// - a transfer: the design, both handles and the home page;
/// - a thumbnail that arrives after the push (the Cloud Tasks retry):
///   every design holding the revision, as a visibility change does,
///   since listings and Open Graph tags show it;
/// - a handle chosen: the handle, since its page starts to exist;
/// - a role change: nothing, since no cacheable page shows roles;
/// - a design's delete: [`of_design`], and its forks' pages with it,
///   since they carry its key;
/// - an account's delete: every design it owned, its handle, and the
///   home page when one of them was public (`crate::delete::account`);
/// - a collector run: nothing, since no branch reaches what it removes;
///   a fork's forked-from line losing a removed revision's hash catches
///   up within the hour ([`Policy::Page`]).
///
/// A design's pages carry its design key alone, so one push does not
/// clear everything its owner has; a fork's also carry its source's,
/// since the forked-from line reads the source's handle and slug.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// The home page's recent list.
    Home,
    /// Every page of one design: the design, branch, revision, history
    /// and schematic pages.
    Design(DesignId),
    /// A person's page, and what answers under the handle for a design
    /// that is not there: a 404, and the redirect from a slug it left.
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

/// What goes in front of every key the server names, in the
/// `Surrogate-Key` header and in a purge alike: `SURROGATE_KEY_PREFIX`,
/// `staging-` on kiewer-staging and nothing in production (#85). Both
/// environments share one Fastly service (ADR 0003) and a purge by key
/// acts on the whole service, so without it a write on staging would drop
/// production's pages that carry the same key, and the reverse.
/// [`KeyPrefix::apply`] is the one place it is applied: the router's
/// layer ([`prefixed`]) for the header, the [`Fastly`] client for the
/// purge. The default is no prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyPrefix(String);

impl KeyPrefix {
    /// `value`, a `Surrogate-Key` value, with the prefix in front of each
    /// of its keys.
    #[must_use]
    pub fn apply(&self, value: &str) -> String {
        if self.0.is_empty() {
            return value.to_owned();
        }
        value
            .split(' ')
            .filter(|key| !key.is_empty())
            .map(|key| format!("{}{key}", self.0))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl FromStr for KeyPrefix {
    type Err = Error;

    /// Visible ASCII with no space: a key is one token of a
    /// space-separated header, so a space would split every key in two.
    /// The empty text is no prefix.
    fn from_str(text: &str) -> Result<Self> {
        if text.bytes().all(|byte| byte.is_ascii_graphic()) {
            Ok(Self(text.to_owned()))
        } else {
            Err(Error::Malformed(format!(
                "surrogate key prefix {text:?} is not visible ASCII without spaces"
            )))
        }
    }
}

/// `response` with `prefix` in front of each key of its `Surrogate-Key`:
/// the layer `crate::web::router` puts over every route, so a handler
/// names its keys as [`page`] and [`not_found_under`] take them and
/// never sees the prefix.
pub async fn prefixed(State(prefix): State<KeyPrefix>, mut response: Response) -> Response {
    let value = response
        .headers()
        .get(SURROGATE_KEY)
        .and_then(|value| value.to_str().ok())
        .map(|value| prefix.apply(value));
    if let Some(value) = value
        && let Ok(value) = HeaderValue::from_str(&value)
    {
        response.headers_mut().insert(SURROGATE_KEY, value);
    }
    response
}

/// What a push or the settings form purges for the design `id` of
/// `owner`: the design, the owner's handle (their page lists it, and a
/// cached 404 under the handle may be this design), and the home page
/// when the design is public.
#[must_use]
pub fn of_design(id: DesignId, owner: &Handle, visibility: Visibility) -> Vec<Key> {
    let mut keys = vec![Key::Design(id), Key::Handle(owner.clone())];
    if visibility == Visibility::Public {
        keys.push(Key::Home);
    }
    keys
}

/// `response` with `policy` and `keys`.
fn with_keys(policy: Policy, keys: &[Key], response: impl IntoResponse) -> Response {
    let mut response = ([policy.header()], response).into_response();
    if !keys.is_empty()
        && let Ok(value) = HeaderValue::from_str(&surrogate_key(keys))
    {
        response.headers_mut().insert(SURROGATE_KEY, value);
    }
    response
}

/// `response` as a cacheable page about `keys`: [`Policy::Page`] and the
/// `Surrogate-Key` header. The page itself must be the same for everyone
/// (ADR 0003): nothing a handler puts in it may come from the session.
pub fn page(keys: &[Key], response: impl IntoResponse) -> Response {
    with_keys(Policy::Page, keys, response)
}

/// A 404 saying `body` for a path nothing can clear early:
/// [`Policy::NotFound`].
pub fn not_found(body: impl Into<String>) -> Response {
    let response = (StatusCode::NOT_FOUND, body.into());
    with_keys(Policy::NotFound, &[], response)
}

/// The same under `segment`, a URL's first segment as it came: when it
/// is a handle the response carries that handle's key, so the write that
/// makes the page exist clears it.
pub fn not_found_under(segment: &str, body: impl Into<String>) -> Response {
    let keys: Vec<Key> = segment.parse().map(Key::Handle).into_iter().collect();
    let response = (StatusCode::NOT_FOUND, body.into());
    with_keys(Policy::NotFound, &keys, response)
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
const FASTLY_API: &str = "https://api.fastly.com";

/// The most keys one purge call takes, as Fastly's purging reference
/// says.
pub const MAX_KEYS: usize = 256;

/// How long one purge call may take: the edge answers in well under a
/// second, and a write's response waits on it.
const PURGE_TIMEOUT: Duration = Duration::from_secs(5);

/// The purge-by-key call of Fastly's API: `POST
/// /service/{service id}/purge` with the token in `Fastly-Key` and the
/// keys in a `Surrogate-Key` request header, which drops them across
/// every POP at once. A late thumbnail on a much-forked revision can
/// name more keys than one call takes, so they go in [`MAX_KEYS`] at a
/// time. Each key goes out with the [`KeyPrefix`] in front, the one the
/// pages' headers carry.
#[derive(Clone)]
pub struct Fastly {
    service_id: String,
    token: String,
    api: String,
    prefix: KeyPrefix,
    http: reqwest::Client,
}

impl fmt::Debug for Fastly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fastly")
            .field("service_id", &self.service_id)
            .field("api", &self.api)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Fastly {
    /// The client for `service_id` with `token`, against Fastly's API.
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
            prefix: KeyPrefix::default(),
            http,
        })
    }

    /// The same client with `prefix` in front of every key it purges.
    #[must_use]
    pub fn with_key_prefix(self, prefix: KeyPrefix) -> Self {
        Self { prefix, ..self }
    }
}

#[async_trait]
impl Purge for Fastly {
    async fn purge(&self, keys: &[Key]) -> Result<()> {
        let url = format!("{}/service/{}/purge", self.api, self.service_id);
        for batch in keys.chunks(MAX_KEYS) {
            let batch = self.prefix.apply(&surrogate_key(batch));
            let response = self
                .http
                .post(&url)
                .header("Fastly-Key", &self.token)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(SURROGATE_KEY, &batch)
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Failed {
                    reason: format!("Fastly answered {status} to the purge of {batch}"),
                });
            }
            tracing::info!(keys = %batch, "purged");
        }
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

    /// Whether `Accept-Encoding` takes the coding `name`: named, or by
    /// `*`, with any weight above zero. No header takes identity alone,
    /// as curl sends.
    #[must_use]
    pub fn accepts(headers: &HeaderMap, name: &str) -> bool {
        headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|coding| {
                let mut parts = coding.split(';');
                let named = parts.next().unwrap_or_default().trim();
                let refused = parts.any(|param| {
                    param
                        .trim()
                        .strip_prefix("q=")
                        .and_then(|q| q.trim().parse::<f32>().ok())
                        .is_some_and(|q| q <= 0.0)
                });
                (named == name || named == "*") && !refused
            })
    }

    /// The coding to send: brotli when it is accepted, else gzip, else
    /// identity.
    #[must_use]
    pub fn negotiate(headers: &HeaderMap) -> Encoding {
        if Self::accepts(headers, "br") {
            Encoding::Brotli
        } else if Self::accepts(headers, "gzip") {
            Encoding::Gzip
        } else {
            Encoding::Identity
        }
    }
}

#[cfg(test)]
mod tests {
    use kiewer_assert::assert_is_empty;
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

    #[test_case("", "design-7 handle-ada home"; "no prefix, production's keys")]
    #[test_case("staging-", "staging-design-7 staging-handle-ada staging-home"; "staging's")]
    fn a_prefix_goes_in_front_of_each_key(prefix: &str, value: &str) {
        let prefix: KeyPrefix = prefix.parse().expect("a prefix");
        assert_eq!(prefix.apply("design-7 handle-ada home"), value);
    }

    #[test_case("staging -"; "a space")]
    #[test_case("staging\t"; "a tab")]
    #[test_case("stagé-"; "a letter beyond ASCII")]
    fn a_prefix_that_would_break_a_key_is_refused(text: &str) {
        text.parse::<KeyPrefix>()
            .expect_err("not visible ASCII without spaces");
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

    #[test]
    fn a_404_is_kept_briefly_and_carries_the_handle_it_is_under() {
        let response = not_found_under("ada", "no such design\n");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "public, max-age=0, s-maxage=60"
        );
        assert_eq!(response.headers().get(SURROGATE_KEY).unwrap(), "handle-ada");
        let unkeyed = not_found_under("Not A Handle", "no such design\n");
        assert!(unkeyed.headers().get(SURROGATE_KEY).is_none());
    }

    #[test_case(Visibility::Public, true; "a public design is on the home page")]
    #[test_case(Visibility::Unlisted, false; "an unlisted one is not")]
    fn a_design_purges_the_home_page_only_when_public(visibility: Visibility, home: bool) {
        let id = DesignId::for_test(9);
        let ada = handle("ada");
        let keys = of_design(id, &ada, visibility);
        assert_eq!(keys[..2], [Key::Design(id), Key::Handle(ada)]);
        assert_eq!(keys.contains(&Key::Home), home);
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
        assert_is_empty!(recorder.calls());
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

    #[test_case(Some("gzip, deflate, br"), true; "a browser")]
    #[test_case(Some("gzip;q=1.0, identity; q=0.5"), true; "weighted")]
    #[test_case(Some("gzip;q=0.5"), true; "a weight that starts with a zero is no refusal")]
    #[test_case(Some("*"), true; "anything")]
    #[test_case(Some("gzip;q=0"), false; "gzip refused")]
    #[test_case(Some("identity"), false; "identity only")]
    #[test_case(None, false; "no header, as curl sends")]
    fn gzip_is_accepted_only_by_a_client_that_takes_it(value: Option<&str>, gzip: bool) {
        assert_eq!(Encoding::accepts(&accept(value), "gzip"), gzip);
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
        let debug = format!("{fastly:?}");
        assert!(debug.contains("SU1Z0isxPaozGVKXdv0eY"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
