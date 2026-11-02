//! The caching policy over HTTP (#26, ADR 0003): every cacheable page
//! renders the same bytes with a session as without one, the Fastly
//! purge client sends the keys to the API's purge-by-key call (a fake
//! of it on loopback, so no network is touched). The Postgres tests skip
//! without `DATABASE_URL`, like `tests/db.rs`; the Fastly fake needs none.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::post;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::{Fastly, Key, Purge};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;

const BOARD: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pcb");
const SCHEMATIC: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_sch");
const PROJECT: &[u8] = include_bytes!("../../viewer-drop/board/midi8knob.kicad_pro");

/// What the fake Fastly saw: one entry per purge call, the service id
/// from the path, the token header and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    service_id: String,
    token: Option<String>,
    keys: Option<String>,
}

type Calls = Arc<Mutex<Vec<Seen>>>;

async fn fake_purge(
    State(calls): State<Calls>,
    Path(service_id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = headers
        .get("fastly-key")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    if token.as_deref() != Some("the-token") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"msg": "Provided credentials are missing or invalid"})),
        );
    }
    let keys = headers
        .get("surrogate-key")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    calls.lock().unwrap().push(Seen {
        service_id,
        token,
        keys,
    });
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

/// A fake of Fastly's API on loopback: its origin and what it saw.
async fn fake_fastly() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let router = Router::new()
        .route("/service/{service_id}/purge", post(fake_purge))
        .with_state(calls.clone());
    let (listener, addr) = http::bind_free_port().await;
    tokio::spawn(async move { axum::serve(listener, router).await });
    (format!("http://{addr}"), calls)
}

#[tokio::test]
async fn the_fastly_client_purges_by_key_with_the_token_and_reports_a_refusal() {
    let (api, calls) = fake_fastly().await;
    let fastly = Fastly::at(&api, "SU1Z0isxPaozGVKXdv0eY", "the-token").expect("a client");
    let ada: Handle = "ada".parse().unwrap();
    fastly
        .purge(&[Key::Home, Key::Handle(ada)])
        .await
        .expect("the API took the purge");
    assert_eq!(
        calls.lock().unwrap().clone(),
        [Seen {
            service_id: "SU1Z0isxPaozGVKXdv0eY".to_owned(),
            token: Some("the-token".to_owned()),
            keys: Some("home handle-ada".to_owned()),
        }]
    );
    fastly
        .purge(&[])
        .await
        .expect("nothing to purge is no call");
    assert_eq!(calls.lock().unwrap().len(), 1);

    let wrong = Fastly::at(&api, "SU1Z0isxPaozGVKXdv0eY", "another").expect("a client");
    let refused = wrong
        .purge(&[Key::Home])
        .await
        .expect_err("the API refused the token");
    assert!(
        refused.to_string().contains("401"),
        "the status is in the reason: {refused}"
    );
}

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "cacher".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("c").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    let id = session::start(db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, signed_in.account_id, handle)
}

/// The hand-off files pushed as a design of the browser's person, at
/// `/{handle}/midi8knob`.
async fn uploaded(browser: &mut http::Browser) -> String {
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("message", "first upload"),
                Part::file("midi8knob.kicad_pcb", BOARD),
                Part::file("midi8knob.kicad_sch", SCHEMATIC),
                Part::file("midi8knob.kicad_pro", PROJECT),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    posted
        .header("location")
        .expect("the design's page")
        .to_owned()
}

/// The first revision hash `body` links under `prefix`.
fn linked_hash(body: &str, prefix: &str) -> String {
    let start = body.find(prefix).expect("a revision link") + prefix.len();
    body[start..start + 64].to_owned()
}

/// What two renders of a page are compared on: everything but the date
/// header.
fn comparable(response: &http::Response) -> (u16, Vec<(String, String)>, String) {
    let headers = response
        .headers
        .iter()
        .filter(|(name, _)| name != "date")
        .cloned()
        .collect();
    (response.status, headers, response.body.clone())
}

#[tokio::test]
async fn every_cacheable_page_renders_the_same_bytes_with_a_session_as_without() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, _, handle) = signed_in(&db, addr).await;
    let design_path = uploaded(&mut owner).await;
    assert_eq!(design_path, format!("/{handle}/midi8knob"));
    let page = http::request(addr, "GET", &design_path, &[], "").await;
    // The revision dropdown's option values are the revision URLs.
    let revision = linked_hash(&page.body, &format!("{design_path}/r/"));
    let board = linked_hash(&page.body, "fetch(\"/blob/");
    // A design with a schematic alone opens on it: the bare URL answers
    // 303 and `?design` keeps the design page.
    let sheet_only = owner
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "sheet only"),
                Part::file("sheet.kicad_sch", SCHEMATIC),
            ],
        )
        .await;
    assert_eq!(sheet_only.status, 303, "{}", sheet_only.body);
    let sheet_only = sheet_only
        .header("location")
        .expect("the design")
        .to_owned();
    let bare = http::request(addr, "GET", &sheet_only, &[], "").await;
    assert_eq!(bare.status, 303, "a schematic alone opens on the schematic");

    let pages = [
        "/".to_owned(),
        "/about".to_owned(),
        "/viewer".to_owned(),
        "/viewer/midi8knob.kicad_pcb".to_owned(),
        "/schematic".to_owned(),
        "/schematic/midi8knob.kicad_sch".to_owned(),
        format!("/{handle}"),
        design_path.clone(),
        format!("{design_path}/b/main"),
        format!("{design_path}/r/{revision}"),
        format!("{design_path}/b/main/history"),
        format!("{design_path}/schematic"),
        format!("/blob/{board}"),
        format!("/{handle}/no-such-design"),
        format!("/{}", fresh("nobody")),
        format!("{design_path}?design"),
        sheet_only.clone(),
        format!("{sheet_only}?design"),
    ];
    for path in &pages {
        let anonymous = http::request(addr, "GET", path, &[], "").await;
        let with_session = owner.get(path).await;
        assert_eq!(
            comparable(&anonymous),
            comparable(&with_session),
            "{path} depends on the session"
        );
        assert!(
            with_session.header("set-cookie").is_none(),
            "{path} touched the session cookie"
        );
    }
}
