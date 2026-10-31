//! Sign-in over HTTP: `/api/me`, the flow with each provider against a
//! fake of it on loopback (an axum server answering the authorize, token
//! and user endpoints on the real paths, so no network is touched), the
//! welcome page's handle form and sign-out. The flow tests need Postgres and skip without
//! `DATABASE_URL`, like `tests/db.rs`; `/api/me` without a session needs
//! no database.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Redirect};
use axum::routing::{get, post};
use kiewer_lib::account::{self, Handle};
use kiewer_lib::auth::{App, Auth, Credentials, Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::session::SessionId;
use kiewer_lib::web::AppState;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::{TempDir, http, scratch_dir};
use oauth2::url::Url;
use oauth2::{PkceCodeChallenge, PkceCodeVerifier};
use sqlx::PgPool;
use test_case::test_case;

/// A provider on loopback: the authorize page sends the browser straight
/// back with a code, the token endpoint checks the code and the PKCE
/// verifier against the challenge the authorize page saw, and the user
/// endpoint answers for the bearer token it issued. Its routes are the
/// real provider's paths, so the app under test is the real one
/// (`Provider::app`) with the host swapped.
struct FakeProvider {
    challenge: Option<String>,
    /// The `scope` the authorize page was asked for, as the provider sees
    /// it: space-separated, or absent.
    scope: Option<String>,
    /// What the user endpoint answers; a test changes it to be someone
    /// else.
    user: serde_json::Value,
}

type Fake = Arc<Mutex<FakeProvider>>;

const FAKE_CODE: &str = "fake-code";
const FAKE_TOKEN: &str = "fake-token";

fn fake_credentials() -> Credentials {
    Credentials {
        client_id: "fake-id".to_owned(),
        client_secret: "fake-secret".to_owned(),
    }
}

async fn fake_authorize(
    State(fake): State<Fake>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    assert_eq!(query.get("client_id").map(String::as_str), Some("fake-id"));
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    {
        let mut fake = fake.lock().unwrap();
        fake.challenge = query.get("code_challenge").cloned();
        fake.scope = query.get("scope").cloned();
    }
    let back = format!(
        "{}?code={FAKE_CODE}&state={}",
        query["redirect_uri"], query["state"]
    );
    Redirect::to(&back)
}

async fn fake_token(
    State(fake): State<Fake>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> impl IntoResponse {
    assert!(
        headers
            .get(header::AUTHORIZATION)
            .is_some_and(|value| value.as_bytes().starts_with(b"Basic ")),
        "the client id and secret come as HTTP basic auth"
    );
    let expected = fake.lock().unwrap().challenge.clone();
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let proven = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier));
    if form.get("code").map(String::as_str) != Some(FAKE_CODE)
        || expected.as_deref() != Some(proven.as_str())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_verification_code"})),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"access_token": FAKE_TOKEN, "token_type": "bearer", "scope": ""})),
    )
}

async fn fake_user(State(fake): State<Fake>, headers: HeaderMap) -> impl IntoResponse {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    if bearer.as_deref() != Some(&format!("Bearer {FAKE_TOKEN}")) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"message": "Bad credentials"})),
        );
    }
    (StatusCode::OK, Json(fake.lock().unwrap().user.clone()))
}

/// The path of an absolute URL: where the fake serves a real endpoint.
fn path_of(url: &str) -> String {
    Url::parse(url).expect("a URL").path().to_owned()
}

/// The fake for `provider` on a free port, answering for `user` at its
/// user endpoint.
async fn fake_provider(provider: Provider, user: serde_json::Value) -> (Fake, SocketAddr) {
    let fake: Fake = Arc::new(Mutex::new(FakeProvider {
        challenge: None,
        scope: None,
        user,
    }));
    let real = provider.app(fake_credentials());
    let router = Router::new()
        .route(&path_of(&real.auth_url), get(fake_authorize))
        .route(&path_of(&real.token_url), post(fake_token))
        .route(&path_of(&real.user_url), get(fake_user))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (fake, addr)
}

/// Digits no earlier run used: a GitHub id, a Discord snowflake, a Google
/// sub.
fn fresh_digits() -> String {
    u64::from_str_radix(&fresh("")[1..13], 16)
        .expect("twelve hex digits")
        .to_string()
}

/// A fake GitHub answering for the user `login` under a numeric id no
/// earlier run used.
async fn fake_github(login: &str) -> (Fake, SocketAddr) {
    let id: u64 = fresh_digits().parse().unwrap();
    fake_provider(
        Provider::GitHub,
        serde_json::json!({"id": id, "login": login}),
    )
    .await
}

/// What a provider's user endpoint says about a fresh person with the
/// login `login` and the verified email `email`, in that provider's shape.
fn fresh_user(provider: Provider, login: &str, email: &str) -> serde_json::Value {
    match provider {
        Provider::GitHub => serde_json::json!({
            "id": fresh_digits().parse::<u64>().unwrap(), "login": login, "email": email
        }),
        Provider::Discord => serde_json::json!({
            "id": fresh_digits(), "username": login, "discriminator": "0",
            "verified": true, "email": email
        }),
        Provider::Google => serde_json::json!({
            "sub": fresh_digits(), "name": login, "email": email, "email_verified": true
        }),
    }
}

/// The real app for `provider` with every endpoint moved to the fake at
/// `addr`: the real scopes and paths, the fake's host.
fn fake_app(provider: Provider, addr: SocketAddr) -> App {
    let real = provider.app(fake_credentials());
    let moved = |url: &str| format!("http://{addr}{}", path_of(url));
    App {
        auth_url: moved(&real.auth_url),
        token_url: moved(&real.token_url),
        user_url: moved(&real.user_url),
        ..real
    }
}

/// The server with sign-in registered for each provider against its fake,
/// on a free port; a browser at it. The public URL is that port under
/// `scheme`: the browser follows paths, so `https` works without TLS and
/// gives the production cookie attributes.
async fn server_with(
    db: PgPool,
    fakes: &[(Provider, SocketAddr)],
    scheme: &str,
) -> (http::Browser, TempDir) {
    let dir = scratch_dir!();
    let (listener, addr) = http::bind_free_port().await;
    let apps = fakes
        .iter()
        .map(|(provider, fake)| fake_app(*provider, *fake))
        .collect();
    let state = AppState {
        db,
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(apps, format!("{scheme}://{addr}")).expect("an auth")),
    };
    http::serve_listener(listener, state);
    (http::Browser::new(addr), dir)
}

/// The server with GitHub alone, against the fake at `github`.
async fn server_with_github(
    db: PgPool,
    github: SocketAddr,
    scheme: &str,
) -> (http::Browser, TempDir) {
    server_with(db, &[(Provider::GitHub, github)], scheme).await
}

/// Every attribute a cookie line has to carry, named in the failure.
fn assert_attributes(cookie: &str, attributes: &[&str]) {
    let missing: Vec<&str> = attributes
        .iter()
        .copied()
        .filter(|attribute| !cookie.contains(attribute))
        .collect();
    assert!(missing.is_empty(), "{cookie} lacks {missing:?}");
}

/// The path and query of an absolute URL, for the next request.
fn path_and_query(url: &str) -> (Url, String) {
    let url: Url = url.parse().expect("an absolute URL");
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    (url, path)
}

/// Drive the browser from `start` (a `/auth/{provider}` URL, perhaps with
/// a query) through the sign-in: the redirect to the fake at `fake`, the
/// fake's redirect back, the callback. Returns the callback's response.
async fn drive(
    browser: &mut http::Browser,
    start: &str,
    provider: Provider,
    fake: SocketAddr,
) -> http::Response {
    let started = browser.get(start).await;
    assert_eq!(started.status, 303, "{}", started.body);
    let state_cookie = started.header("set-cookie").expect("the state cookie");
    assert!(state_cookie.starts_with("kiewer_oauth="), "{state_cookie}");
    assert_attributes(state_cookie, &["HttpOnly", "SameSite=Lax", "Path=/auth"]);
    assert_eq!(started.header("cache-control"), Some("private, no-store"));
    let (authorize, authorize_path) = path_and_query(started.header("location").unwrap());
    assert_eq!(
        authorize.port(),
        Some(fake.port()),
        "sent to the fake {provider}"
    );
    let back = http::request(fake, "GET", &authorize_path, &[], "").await;
    assert_eq!(back.status, 303, "{}", back.body);
    let (callback, callback_path) = path_and_query(back.header("location").unwrap());
    assert_eq!(
        callback.port(),
        Some(browser.addr().port()),
        "sent back to kiewer"
    );
    assert_eq!(callback.path(), format!("/auth/{provider}/callback"));
    browser.get(&callback_path).await
}

/// A plain sign-in with `provider` against the fake at `fake`.
async fn sign_in_with(
    browser: &mut http::Browser,
    provider: Provider,
    fake: SocketAddr,
) -> http::Response {
    drive(browser, &format!("/auth/{provider}"), provider, fake).await
}

/// A plain sign-in with GitHub against the fake at `github`.
async fn sign_in_through(browser: &mut http::Browser, github: SocketAddr) -> http::Response {
    sign_in_with(browser, Provider::GitHub, github).await
}

async fn me(browser: &mut http::Browser) -> serde_json::Value {
    let response = browser.get("/api/me").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    serde_json::from_str(&response.body).expect("json")
}

#[tokio::test]
async fn a_person_signs_in_picks_a_handle_stays_signed_in_and_signs_out() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("Octo_Cat").await;
    let (mut browser, _dir) = server_with_github(db.clone(), github, "https").await;

    let started = browser.get("/auth/github").await;
    assert_attributes(
        started.header("set-cookie").expect("the state cookie"),
        &["Secure"],
    );
    let signed_in = sign_in_through(&mut browser, github).await;
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    assert_eq!(
        signed_in.header("location"),
        Some("/welcome"),
        "no handle yet"
    );
    let cookies = signed_in.headers_named("set-cookie");
    let session_cookie = cookies
        .iter()
        .find(|cookie| cookie.starts_with("kiewer_session="))
        .expect("the session cookie");
    assert_attributes(
        session_cookie,
        &[
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/",
            "Max-Age=2592000",
        ],
    );
    assert_eq!(signed_in.header("cache-control"), Some("private, no-store"));
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("kiewer_oauth=") && cookie.contains("Max-Age=0")),
        "the state cookie is forgotten: {cookies:?}"
    );
    assert_eq!(
        me(&mut browser).await,
        serde_json::json!({"signed_in": true, "handle": null})
    );

    let handle = pick_a_handle(&mut browser, &db).await;
    assert_eq!(
        me(&mut browser).await,
        serde_json::json!({"signed_in": true, "handle": handle}),
        "a reload still finds the session"
    );
    let again = browser.get("/welcome").await;
    assert_eq!(again.status, 303, "the handle is chosen once");

    let signed_out = browser.post_form("/auth/sign-out", "").await;
    assert_eq!(signed_out.status, 303, "{}", signed_out.body);
    let forgotten = signed_out.header("set-cookie").expect("the cookie removal");
    assert!(
        forgotten.starts_with("kiewer_session=") && forgotten.contains("Max-Age=0"),
        "{forgotten}"
    );
    assert!(browser.cookie("kiewer_session").is_none());
    assert_eq!(
        me(&mut browser).await,
        serde_json::json!({"signed_in": false, "handle": null})
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session s JOIN account a ON a.id = s.account_id WHERE a.handle = $1",
    )
    .bind(&handle)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(rows, 0, "sign-out ends the session row");

    // The same person again: the identity finds the account, no welcome.
    let back = sign_in_through(&mut browser, github).await;
    assert_eq!(back.status, 303, "{}", back.body);
    assert_eq!(back.header("location"), Some("/"));
    assert_eq!(me(&mut browser).await["handle"], handle);
}

/// The welcome page for a browser whose GitHub login is `Octo_Cat`: the
/// prefill, a malformed attempt, a taken one, then a free one, which is
/// returned.
async fn pick_a_handle(browser: &mut http::Browser, db: &PgPool) -> String {
    let welcome = browser.get("/welcome").await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    assert_eq!(welcome.header("cache-control"), Some("private, no-store"));
    assert!(
        welcome.body.contains(r#"value="octo-cat""#),
        "prefilled from the login: {}",
        welcome.body
    );

    let refused = browser.post_form("/welcome", "handle=Octo+Cat").await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(
        refused
            .body
            .contains("lowercase letters, digits and hyphens"),
        "{}",
        refused.body
    );

    let other = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "someone".to_owned(),
            email: None,
        },
    )
    .await
    .unwrap();
    let taken: Handle = fresh("taken").parse().unwrap();
    account::choose_handle(db, other.account_id, &taken)
        .await
        .unwrap();
    let refused = browser
        .post_form("/welcome", &format!("handle={taken}"))
        .await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(refused.body.contains("is taken"), "{}", refused.body);
    assert!(
        refused.body.contains(&format!(r#"value="{taken}""#)),
        "the attempt stays in the field"
    );

    let handle = fresh("octo");
    let chosen = browser
        .post_form("/welcome", &format!("handle={handle}"))
        .await;
    assert_eq!(chosen.status, 303, "{}", chosen.body);
    assert_eq!(chosen.header("location"), Some("/"));
    handle
}

#[test_case(Provider::Discord, "identify email"; "discord with identify and email")]
#[test_case(Provider::Google, "openid email profile"; "google with openid email and profile")]
#[tokio::test]
async fn a_person_signs_in_with_another_provider(provider: Provider, scope: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let email = format!("{}@example.com", fresh("mail"));
    let (fake, addr) = fake_provider(provider, fresh_user(provider, "Nelly", &email)).await;
    let (mut browser, _dir) = server_with(db.clone(), &[(provider, addr)], "https").await;

    let signed_in = sign_in_with(&mut browser, provider, addr).await;
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    assert_eq!(signed_in.header("location"), Some("/welcome"));
    assert_eq!(
        fake.lock().unwrap().scope.as_deref(),
        Some(scope),
        "the scopes the provider was asked for"
    );
    assert_eq!(
        me(&mut browser).await,
        serde_json::json!({"signed_in": true, "handle": null})
    );
    let welcome = browser.get("/welcome").await;
    assert!(
        welcome.body.contains(r#"value="nelly""#),
        "prefilled from the provider's login: {}",
        welcome.body
    );
    let row: (String, String, Option<String>) =
        sqlx::query_as("SELECT provider, provider_login, email FROM identity WHERE email = $1")
            .bind(&email)
            .fetch_one(&db)
            .await
            .expect("the identity row");
    assert_eq!(
        row,
        (
            provider.to_string(),
            "Nelly".to_owned(),
            Some(email.clone())
        )
    );
}

#[tokio::test]
async fn the_front_page_links_only_the_registered_providers() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("x").await;
    let (_discord, discord) =
        fake_provider(Provider::Discord, fresh_user(Provider::Discord, "x", "x@x")).await;
    let (mut browser, _dir) = server_with(
        db,
        &[(Provider::GitHub, github), (Provider::Discord, discord)],
        "https",
    )
    .await;
    let page = browser.get("/").await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains(r#"href="/auth/github""#),
        "{}",
        page.body
    );
    assert!(page.body.contains("Sign in with Discord"), "{}", page.body);
    assert!(
        !page.body.contains("/auth/google"),
        "Google has no app here: absent, not broken: {}",
        page.body
    );
}

/// Sign in with GitHub as `login` and pick a fresh handle; the handle.
async fn sign_up_with_github(browser: &mut http::Browser, github: SocketAddr) -> String {
    let signed_in = sign_in_through(browser, github).await;
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    let handle = fresh("h");
    let chosen = browser
        .post_form("/welcome", &format!("handle={handle}"))
        .await;
    assert_eq!(chosen.status, 303, "{}", chosen.body);
    handle
}

#[test_case(Provider::Discord; "discord")]
#[test_case(Provider::Google; "google")]
#[tokio::test]
async fn a_signed_in_person_links_another_provider_from_settings_and_signs_in_with_it(
    provider: Provider,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("Octo").await;
    let email = format!("{}@example.com", fresh("octo"));
    let (_other, other) = fake_provider(provider, fresh_user(provider, "octo_d", &email)).await;
    // The third provider has no app here.
    let absent = Provider::ALL
        .into_iter()
        .find(|p| *p != Provider::GitHub && *p != provider)
        .unwrap();
    let (mut browser, _dir) = server_with(
        db.clone(),
        &[(Provider::GitHub, github), (provider, other)],
        "https",
    )
    .await;
    let handle = sign_up_with_github(&mut browser, github).await;

    let settings = browser.get("/settings").await;
    assert_eq!(settings.status, 200, "{}", settings.body);
    assert_eq!(settings.header("cache-control"), Some("private, no-store"));
    assert!(settings.body.contains(&handle), "{}", settings.body);
    assert!(
        settings.body.contains("GitHub") && settings.body.contains("Octo"),
        "the linked identity is listed: {}",
        settings.body
    );
    assert!(
        settings
            .body
            .contains(&format!(r#"href="/auth/{provider}?intent=link""#)),
        "{provider} is registered and not linked: offered: {}",
        settings.body
    );
    assert!(
        !settings.body.contains(r#"intent=link">Link GitHub"#),
        "GitHub is linked already: not offered: {}",
        settings.body
    );
    assert!(
        !settings.body.contains(&format!("/auth/{absent}")),
        "{absent} has no app: absent: {}",
        settings.body
    );

    let linked = drive(
        &mut browser,
        &format!("/auth/{provider}?intent=link"),
        provider,
        other,
    )
    .await;
    assert_eq!(linked.status, 303, "{}", linked.body);
    assert_eq!(linked.header("location"), Some("/settings"));
    assert!(browser.cookie("kiewer_oauth").is_none());
    let settings = browser.get("/settings").await;
    assert!(
        settings.body.contains(provider.name())
            && settings.body.contains("octo_d")
            && settings.body.contains(&email),
        "the new identity is listed with its login and email: {}",
        settings.body
    );
    assert!(
        !settings.body.contains("intent=link"),
        "every registered provider is linked: nothing to offer: {}",
        settings.body
    );
    assert_eq!(
        me(&mut browser).await["handle"],
        handle,
        "linking kept the session"
    );

    // A sign-in with the linked provider lands on the same account.
    browser.post_form("/auth/sign-out", "").await;
    assert_eq!(me(&mut browser).await["signed_in"], false);
    let back = sign_in_with(&mut browser, provider, other).await;
    assert_eq!(back.status, 303, "{}", back.body);
    assert_eq!(back.header("location"), Some("/"), "a handle already");
    assert_eq!(me(&mut browser).await["handle"], handle);
}

#[tokio::test]
async fn a_link_whose_session_ended_on_the_way_is_refused_and_a_declined_one_returns_to_settings() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("olga").await;
    let (_discord, discord) = fake_provider(
        Provider::Discord,
        fresh_user(Provider::Discord, "olga_d", "olga@example.com"),
    )
    .await;
    let (mut browser, _dir) = server_with(
        db,
        &[(Provider::GitHub, github), (Provider::Discord, discord)],
        "https",
    )
    .await;
    sign_up_with_github(&mut browser, github).await;

    let started = browser.get("/auth/discord?intent=link").await;
    assert_eq!(started.status, 303, "{}", started.body);
    let (_, authorize_path) = path_and_query(started.header("location").unwrap());
    let back = http::request(discord, "GET", &authorize_path, &[], "").await;
    let (_, callback_path) = path_and_query(back.header("location").unwrap());
    // The session ends while the browser is at the provider.
    browser.post_form("/auth/sign-out", "").await;
    let refused = browser.get(&callback_path).await;
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(refused.body.contains("sign in"), "{}", refused.body);
    assert_eq!(refused.header("cache-control"), Some("private, no-store"));
    assert!(
        browser.cookie("kiewer_session").is_none(),
        "a dead link is not made a sign-in"
    );

    // Declining at the provider during a link goes back to settings.
    sign_in_through(&mut browser, github).await;
    browser.get("/auth/discord?intent=link").await;
    let declined = browser
        .get("/auth/discord/callback?error=access_denied")
        .await;
    assert_eq!(declined.status, 303, "{}", declined.body);
    assert_eq!(declined.header("location"), Some("/settings"));
    assert_eq!(me(&mut browser).await["signed_in"], true);
}

#[tokio::test]
async fn an_identity_that_signs_in_elsewhere_is_not_linked_and_the_page_says_so() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("mallory").await;
    let (_google, google) = fake_provider(
        Provider::Google,
        fresh_user(Provider::Google, "Bob", "bob@example.com"),
    )
    .await;
    let (mut mallory, _dir) = server_with(
        db.clone(),
        &[(Provider::GitHub, github), (Provider::Google, google)],
        "https",
    )
    .await;
    // Bob signs in with Google in his own browser: his account.
    let mut bob = http::Browser::new(mallory.addr());
    let bobs = sign_in_with(&mut bob, Provider::Google, google).await;
    assert_eq!(bobs.status, 303, "{}", bobs.body);

    sign_up_with_github(&mut mallory, github).await;
    let refused = drive(
        &mut mallory,
        "/auth/google?intent=link",
        Provider::Google,
        google,
    )
    .await;
    assert_eq!(refused.status, 303, "{}", refused.body);
    assert_eq!(refused.header("location"), Some("/settings?refused=google"));
    let settings = mallory.get("/settings").await;
    assert_eq!(settings.status, 200);
    let settings = mallory.get("/settings?refused=google").await;
    assert_eq!(settings.status, 200, "{}", settings.body);
    assert!(
        settings.body.contains("another kiewer account"),
        "the refusal is said: {}",
        settings.body
    );
    assert!(
        !settings.body.contains("Bob"),
        "nothing of bob's is on mallory's page: {}",
        settings.body
    );
    assert_eq!(me(&mut bob).await["signed_in"], true, "bob is untouched");
}

#[tokio::test]
async fn a_link_needs_a_signed_in_browser_and_settings_need_a_session() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("nobody").await;
    let (mut browser, _dir) = server_with_github(db, github, "https").await;
    let cold = browser.get("/auth/github?intent=link").await;
    assert_eq!(cold.status, 400, "{}", cold.body);
    assert!(cold.body.contains("sign in"), "{}", cold.body);
    assert!(browser.cookie("kiewer_oauth").is_none(), "no flow started");
    let settings = browser.get("/settings").await;
    assert_eq!(settings.status, 303, "{}", settings.body);
    assert_eq!(settings.header("location"), Some("/"));
    assert_eq!(settings.header("cache-control"), Some("private, no-store"));
    // Signed in without a handle: the welcome page comes first.
    sign_in_through(&mut browser, github).await;
    let settings = browser.get("/settings").await;
    assert_eq!(settings.status, 303, "{}", settings.body);
    assert_eq!(settings.header("location"), Some("/welcome"));
}

/// The key of the session behind the browser's cookie, as the sessions
/// page names it.
fn session_key_of(browser: &http::Browser) -> String {
    browser
        .cookie("kiewer_session")
        .expect("signed in")
        .parse::<SessionId>()
        .expect("a session id")
        .key()
        .to_string()
}

#[tokio::test]
async fn the_sessions_page_lists_the_accounts_sessions_and_revokes_them() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("judy").await;
    let (mut laptop, _dir) = server_with_github(db, github, "https").await;
    sign_up_with_github(&mut laptop, github).await;
    // The same person in two more browsers.
    let mut phone = http::Browser::new(laptop.addr());
    let mut tablet = http::Browser::new(laptop.addr());
    for browser in [&mut phone, &mut tablet] {
        let signed_in = sign_in_through(browser, github).await;
        assert_eq!(signed_in.header("location"), Some("/"), "a handle already");
    }

    let page = laptop.get("/settings/sessions").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(page.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        page.body.matches("<time datetime=\"").count(),
        6,
        "a created and a last-seen time per session: {}",
        page.body
    );
    let shown = page
        .body
        .split("<time datetime=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a time");
    assert!(
        shown.len() == 20 && shown.as_bytes()[10] == b'T' && shown.ends_with('Z'),
        "ISO 8601 UTC to the second: {shown}"
    );
    assert_eq!(
        page.body.matches("This browser").count(),
        1,
        "the current session is marked once: {}",
        page.body
    );
    assert!(
        page.body.contains(&session_key_of(&phone)) && page.body.contains(&session_key_of(&tablet)),
        "the others are named by their keys, revocable: {}",
        page.body
    );
    assert!(
        !page.body.contains(laptop.cookie("kiewer_session").unwrap()),
        "the cookie's id is never on the page"
    );

    // Revoke the phone from the laptop.
    let revoked = laptop
        .post_form(
            "/settings/sessions/revoke",
            &format!("key={}", session_key_of(&phone)),
        )
        .await;
    assert_eq!(revoked.status, 303, "{}", revoked.body);
    assert_eq!(revoked.header("location"), Some("/settings/sessions"));
    assert_eq!(revoked.header("cache-control"), Some("private, no-store"));
    assert_eq!(me(&mut phone).await["signed_in"], false, "the phone is out");
    assert_eq!(me(&mut tablet).await["signed_in"], true);
    assert_eq!(me(&mut laptop).await["signed_in"], true);

    // Revoke all others from the laptop: the tablet goes, the laptop stays.
    let others = laptop
        .post_form("/settings/sessions/revoke-others", "")
        .await;
    assert_eq!(others.status, 303, "{}", others.body);
    assert_eq!(others.header("location"), Some("/settings/sessions"));
    assert_eq!(others.header("cache-control"), Some("private, no-store"));
    assert_eq!(me(&mut tablet).await["signed_in"], false);
    assert_eq!(me(&mut laptop).await["signed_in"], true);
    let page = laptop.get("/settings/sessions").await;
    assert_eq!(
        page.body.matches("<time datetime=\"").count(),
        2,
        "{}",
        page.body
    );
}

#[tokio::test]
async fn a_revoke_takes_the_accounts_own_keys_and_a_session() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_github, github) = fake_github("lena").await;
    let (mut laptop, _dir) = server_with_github(db.clone(), github, "https").await;
    sign_up_with_github(&mut laptop, github).await;

    // A key that is not one is refused; a stranger's key revokes nothing.
    let bad = laptop
        .post_form("/settings/sessions/revoke", "key=not-a-key")
        .await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    assert_eq!(bad.header("cache-control"), Some("private, no-store"));
    let (_other_github, other_github) = fake_github("kim").await;
    let (mut kim, _dir2) = server_with_github(db, other_github, "https").await;
    sign_in_through(&mut kim, other_github).await;
    let someone_elses = laptop
        .post_form(
            "/settings/sessions/revoke",
            &format!("key={}", session_key_of(&kim)),
        )
        .await;
    assert_eq!(someone_elses.status, 303);
    assert_eq!(me(&mut kim).await["signed_in"], true, "kim is untouched");

    // Revoking the current session is a sign-out.
    let own = laptop
        .post_form(
            "/settings/sessions/revoke",
            &format!("key={}", session_key_of(&laptop)),
        )
        .await;
    assert_eq!(own.status, 303, "{}", own.body);
    assert_eq!(own.header("location"), Some("/"));
    assert!(
        laptop.cookie("kiewer_session").is_none(),
        "the cookie is forgotten"
    );
    assert_eq!(me(&mut laptop).await["signed_in"], false);

    // Signed out: the page and its forms send the browser home.
    let page = laptop.get("/settings/sessions").await;
    assert_eq!(page.status, 303);
    assert_eq!(page.header("location"), Some("/"));
    let post = laptop
        .post_form("/settings/sessions/revoke-others", "")
        .await;
    assert_eq!(post.status, 303);
    assert_eq!(post.header("location"), Some("/"));
}

#[tokio::test]
async fn a_callback_without_this_browsers_state_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("mallory").await;
    let (mut browser, _dir) = server_with_github(db, github, "https").await;
    let started = browser.get("/auth/github").await;
    assert_eq!(started.status, 303);
    let forged = browser
        .get("/auth/github/callback?code=fake-code&state=not-the-one-in-the-cookie")
        .await;
    assert_eq!(forged.status, 400, "{}", forged.body);
    assert!(
        forged
            .headers_named("set-cookie")
            .iter()
            .all(|c| !c.starts_with("kiewer_session="))
    );
    assert_eq!(me(&mut browser).await["signed_in"], false);
    // No cookie at all: the same refusal.
    let mut fresh_browser = http::Browser::new(browser.addr());
    let cold = fresh_browser
        .get("/auth/github/callback?code=fake-code&state=x")
        .await;
    assert_eq!(cold.status, 400, "{}", cold.body);
}

#[tokio::test]
async fn a_declined_sign_in_goes_home_without_a_session() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("shy").await;
    let (mut browser, _dir) = server_with_github(db, github, "https").await;
    browser.get("/auth/github").await;
    let declined = browser
        .get("/auth/github/callback?error=access_denied&error_description=The+user+has+denied")
        .await;
    assert_eq!(declined.status, 303, "{}", declined.body);
    assert_eq!(declined.header("location"), Some("/"));
    assert!(browser.cookie("kiewer_session").is_none());
    assert!(
        browser.cookie("kiewer_oauth").is_none(),
        "the state cookie is forgotten"
    );
}

#[tokio::test]
async fn a_bad_code_at_the_provider_is_an_upstream_failure_not_a_session() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("eve").await;
    let (mut browser, _dir) = server_with_github(db, github, "https").await;
    let started = browser.get("/auth/github").await;
    let state_value = browser
        .cookie("kiewer_oauth")
        .unwrap()
        .split_once('.')
        .unwrap()
        .0
        .to_owned();
    assert_eq!(started.status, 303);
    // A code the provider never issued, with the right state.
    let failed = browser
        .get(&format!(
            "/auth/github/callback?code=stolen&state={state_value}"
        ))
        .await;
    assert_eq!(failed.status, 502, "{}", failed.body);
    assert!(failed.body.contains("start again"), "{}", failed.body);
    assert_eq!(failed.header("cache-control"), Some("private, no-store"));
    assert!(browser.cookie("kiewer_session").is_none());
}

#[tokio::test]
async fn a_use_after_a_day_sets_the_cookie_again_with_the_full_lifetime() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("frank").await;
    let (mut browser, _dir) = server_with_github(db.clone(), github, "https").await;
    sign_in_through(&mut browser, github).await;
    let cookie = browser
        .cookie("kiewer_session")
        .expect("signed in")
        .to_owned();
    let quiet = browser.get("/api/me").await;
    assert!(
        quiet.header("set-cookie").is_none(),
        "a fresh session has nothing to renew"
    );
    // Twenty of thirty days left: in place of a clock, as tests/accounts.rs.
    let aged = sqlx::query(
        "UPDATE session SET expires_at = now() + interval '20 days' WHERE id_hash = sha256($1)",
    )
    .bind(cookie.clone().into_bytes())
    .execute(&db)
    .await
    .expect("age the row")
    .rows_affected();
    assert_eq!(aged, 1);
    let renewed = browser.get("/api/me").await;
    let set = renewed.header("set-cookie").expect("the cookie set again");
    assert!(
        set.starts_with(&format!("kiewer_session={cookie};")),
        "{set}"
    );
    assert_attributes(
        set,
        &["Max-Age=2592000", "HttpOnly", "Secure", "SameSite=Lax"],
    );
    let again = browser.get("/api/me").await;
    assert!(again.header("set-cookie").is_none(), "renewed once a day");
}

#[tokio::test]
async fn cookies_on_an_http_dev_box_are_not_marked_secure() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (_fake, github) = fake_github("dev").await;
    let (mut browser, _dir) = server_with_github(db, github, "http").await;
    let started = browser.get("/auth/github").await;
    let state_cookie = started.header("set-cookie").expect("the state cookie");
    assert_attributes(state_cookie, &["HttpOnly", "SameSite=Lax", "Path=/auth"]);
    assert!(!state_cookie.contains("Secure"), "{state_cookie}");
    assert_eq!(started.header("cache-control"), Some("private, no-store"));
}

/// A state with no provider registered and a pool that never connects.
fn anonymous_state() -> (AppState, TempDir) {
    let dir = scratch_dir!();
    let state = AppState {
        db: kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    (state, dir)
}

#[tokio::test]
async fn me_is_nobody_without_a_cookie_and_never_cached() {
    let (state, _dir) = anonymous_state();
    let addr = http::serve_on_free_port(state).await;
    let response = http::request(addr, "GET", "/api/me", &[], "").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "{:?}",
        response.headers
    );
    let me: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(me, serde_json::json!({"signed_in": false, "handle": null}));
}

#[tokio::test]
async fn me_ignores_a_cookie_that_is_not_a_session_id() {
    let (state, _dir) = anonymous_state();
    let addr = http::serve_on_free_port(state).await;
    let response = http::request(
        addr,
        "GET",
        "/api/me",
        &[("Cookie", "kiewer_session=not-a-session-id")],
        "",
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let me: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(me["signed_in"], false);
}

#[test_case("github"; "github without an app")]
#[test_case("discord"; "discord without an app")]
#[test_case("google"; "google without an app")]
#[test_case("myspace"; "not a provider")]
#[tokio::test]
async fn sign_in_with_an_unregistered_provider_is_not_found(provider: &str) {
    let (state, _dir) = anonymous_state();
    let addr = http::serve_on_free_port(state).await;
    let (status, _) = http::get(addr, &format!("/auth/{provider}")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_front_page_offers_no_sign_in_without_a_provider() {
    let (state, _dir) = anonymous_state();
    let addr = http::serve_on_free_port(state).await;
    let (status, body) = http::get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(
        !body.contains("Sign in with"),
        "no provider, no link: {body}"
    );
}
