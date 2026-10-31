//! Sign-in over HTTP: `/api/me`, the GitHub flow against a fake GitHub on
//! loopback (an axum server answering the authorize, token and user
//! endpoints, so no network is touched), the welcome page's handle form
//! and sign-out. The flow tests need Postgres and skip without
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
use kiewer_lib::auth::{App, Auth, Provider};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::AppState;
use kiewer_test_support::{TempDir, http, scratch_dir};
use oauth2::url::Url;
use oauth2::{PkceCodeChallenge, PkceCodeVerifier};
use sqlx::PgPool;

/// A GitHub on loopback: the authorize page sends the browser straight
/// back with a code, the token endpoint checks the code and the PKCE
/// verifier against the challenge the authorize page saw, and the user
/// endpoint answers for the bearer token it issued.
#[derive(Default)]
struct FakeGitHub {
    challenge: Option<String>,
    /// What `/user` answers; a test changes it to be someone else.
    user: serde_json::Value,
}

type Fake = Arc<Mutex<FakeGitHub>>;

const FAKE_CODE: &str = "fake-code";
const FAKE_TOKEN: &str = "fake-token";

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
    fake.lock().unwrap().challenge = query.get("code_challenge").cloned();
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

/// The fake on a free port, answering for the user `login` with id `id`.
async fn fake_github(id: u64, login: &str) -> (Fake, SocketAddr) {
    let fake: Fake = Arc::new(Mutex::new(FakeGitHub {
        challenge: None,
        user: serde_json::json!({"id": id, "login": login}),
    }));
    let router = Router::new()
        .route("/login/oauth/authorize", get(fake_authorize))
        .route("/login/oauth/access_token", post(fake_token))
        .route("/user", get(fake_user))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (fake, addr)
}

/// The server with GitHub sign-in registered against the fake at
/// `github`, on a free port that is also its public URL; a browser at it.
async fn server_with_github(db: PgPool, github: SocketAddr) -> (http::Browser, TempDir) {
    let dir = scratch_dir!();
    let (listener, addr) = http::bind_free_port().await;
    let app = App {
        provider: Provider::GitHub,
        client_id: "fake-id".to_owned(),
        client_secret: "fake-secret".to_owned(),
        auth_url: format!("http://{github}/login/oauth/authorize"),
        token_url: format!("http://{github}/login/oauth/access_token"),
        user_url: format!("http://{github}/user"),
        scopes: Vec::new(),
    };
    let state = AppState {
        db,
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(vec![app], format!("http://{addr}")).expect("an auth")),
    };
    http::serve_listener(listener, state);
    (http::Browser::new(addr), dir)
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty());
    let Some(url) = url else {
        eprintln!("DATABASE_URL is unset; skipping");
        return None;
    };
    Some(
        kiewer_lib::db::connect(&url)
            .await
            .expect("connect and migrate"),
    )
}

/// A token no earlier run used: rows from earlier runs are still there.
fn fresh(prefix: &str) -> String {
    format!("{prefix}-{:016x}", rand::random::<u64>())
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

/// Drive the browser through the sign-in: the redirect to the fake, the
/// fake's redirect back, the callback. Returns the callback's response.
async fn sign_in_through(browser: &mut http::Browser, github: SocketAddr) -> http::Response {
    let started = browser.get("/auth/github").await;
    assert_eq!(started.status, 303, "{}", started.body);
    let state_cookie = started.header("set-cookie").expect("the state cookie");
    assert!(state_cookie.starts_with("kiewer_oauth="), "{state_cookie}");
    for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/auth"] {
        assert!(state_cookie.contains(attribute), "{state_cookie}");
    }
    let (authorize, authorize_path) = path_and_query(started.header("location").unwrap());
    assert_eq!(
        authorize.port(),
        Some(github.port()),
        "sent to the fake GitHub"
    );
    let back = http::request(github, "GET", &authorize_path, &[], "").await;
    assert_eq!(back.status, 303, "{}", back.body);
    let (callback, callback_path) = path_and_query(back.header("location").unwrap());
    assert_eq!(
        callback.port(),
        Some(browser.addr().port()),
        "sent back to kiewer"
    );
    assert_eq!(callback.path(), "/auth/github/callback");
    browser.get(&callback_path).await
}

async fn me(browser: &mut http::Browser) -> serde_json::Value {
    let response = browser.get("/api/me").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("private, no-store"));
    serde_json::from_str(&response.body).expect("json")
}

#[tokio::test]
async fn a_person_signs_in_picks_a_handle_stays_signed_in_and_signs_out() {
    let Some(db) = pool().await else { return };
    let (_fake, github) = fake_github(rand::random::<u32>().into(), "Octo_Cat").await;
    let (mut browser, _dir) = server_with_github(db.clone(), github).await;

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
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=2592000",
    ] {
        assert!(session_cookie.contains(attribute), "{session_cookie}");
    }
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

    let other = account::sign_in(db, Provider::GitHub, &fresh("gh"), "someone")
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

#[tokio::test]
async fn a_callback_without_this_browsers_state_is_refused() {
    let Some(db) = pool().await else { return };
    let (_fake, github) = fake_github(rand::random::<u32>().into(), "mallory").await;
    let (mut browser, _dir) = server_with_github(db, github).await;
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
    let Some(db) = pool().await else { return };
    let (_fake, github) = fake_github(rand::random::<u32>().into(), "shy").await;
    let (mut browser, _dir) = server_with_github(db, github).await;
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
    let Some(db) = pool().await else { return };
    let (_fake, github) = fake_github(rand::random::<u32>().into(), "eve").await;
    let (mut browser, _dir) = server_with_github(db, github).await;
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
    assert_eq!(failed.status, 500, "{}", failed.body);
    assert!(browser.cookie("kiewer_session").is_none());
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

#[tokio::test]
async fn sign_in_with_an_unregistered_provider_is_not_found() {
    let (state, _dir) = anonymous_state();
    let addr = http::serve_on_free_port(state).await;
    let (status, _) = http::get(addr, "/auth/github").await;
    assert_eq!(status, 404);
    let (status, _) = http::get(addr, "/auth/myspace").await;
    assert_eq!(status, 404);
}
