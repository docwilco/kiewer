//! The caching policy over HTTP (#26, ADR 0003): every cacheable page
//! renders the same bytes with a session as without one, the Fastly
//! purge client sends the keys to the API's purge-by-key call (a fake
//! of it on loopback, so no network is touched), every kind of route
//! names its policy and its keys, and each write purges the keys of the
//! pages it changed. The Postgres tests skip without `DATABASE_URL`, like
//! `tests/db.rs`; the Fastly fake needs none.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::post;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::BlobHash;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::{Fastly, Key, MAX_KEYS, Policy, Purge, Recorder, surrogate_key};
use kiewer_lib::design::{self, Design};
use kiewer_lib::roles::{self, Visibility};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

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

    calls.lock().unwrap().clear();
    let many: Vec<Key> = (0..=MAX_KEYS)
        .map(|n| Key::Handle(format!("person-{n}").parse().unwrap()))
        .collect();
    fastly.purge(&many).await.expect("the API took both calls");
    let keys_per_call: Vec<usize> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|seen| seen.keys.as_deref().unwrap_or("").split(' ').count())
        .collect();
    assert_eq!(keys_per_call, [MAX_KEYS, 1], "more than one call takes");

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

/// What a page test looks at: the owner's browser, a design of the
/// hand-off files and one with a schematic alone.
struct Site {
    addr: SocketAddr,
    owner: http::Browser,
    handle: Handle,
    path: String,
    revision: String,
    board: String,
    nobody: String,
    design: Design,
    _dir: TempDir,
}

impl Site {
    async fn up(db: PgPool) -> Site {
        let (addr, dir) = server(db.clone()).await;
        let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
        let path = uploaded(&mut owner).await;
        assert_eq!(path, format!("/{handle}/midi8knob"));
        let design = only_design(&db, owner_id).await;
        let page = http::request(addr, "GET", &path, &[], "").await;
        // The revision dropdown's option values are the revision URLs.
        let revision = linked_hash(&page.body, &format!("{path}/r/"));
        let board = linked_hash(&page.body, "fetch(\"/blob/");
        // A design with a schematic alone opens on it: the bare URL
        // answers 303 and `?design` keeps the design page.
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
        Site {
            addr,
            owner,
            handle,
            path,
            revision,
            board,
            nobody: fresh("nobody"),
            design,
            _dir: dir,
        }
    }

    /// `template` with this site's values in place of its braces.
    fn url(&self, template: &str) -> String {
        template
            .replace("{path}", &self.path)
            .replace("{handle}", self.handle.as_str())
            .replace("{revision}", &self.revision)
            .replace("{board}", &self.board)
            .replace("{nobody}", &self.nobody)
            .replace("{design_key}", &Key::Design(self.design.id).to_string())
    }
}

#[test_case("/"; "the home page")]
#[test_case("/about"; "the about page")]
#[test_case("/viewer"; "the hand-off viewer")]
#[test_case("/viewer/midi8knob.kicad_pcb"; "the hand-off board")]
#[test_case("/schematic"; "the hand-off schematic")]
#[test_case("/schematic/midi8knob.kicad_sch"; "the hand-off sheet")]
#[test_case("/{handle}"; "a person")]
#[test_case("{path}"; "a design")]
#[test_case("{path}/b/main"; "a branch")]
#[test_case("{path}/r/{revision}"; "a revision")]
#[test_case("{path}/b/main/history"; "a history")]
#[test_case("{path}/branches"; "the branches page")]
#[test_case("{path}/schematic"; "a schematic")]
#[test_case("{path}?design"; "a design asked to stay")]
#[test_case("{path}/r/{revision}/f/midi8knob.kicad_pcb"; "a file")]
#[test_case("{path}/r/{revision}/thumbnail/400x400.png"; "a thumbnail")]
#[test_case("/blob/{board}"; "a blob")]
#[test_case("/{handle}/no-such-design"; "a design that is not there")]
#[test_case("/{nobody}"; "a person who is not there")]
#[test_case("/{handle}/sheet-only"; "a schematic alone, which redirects")]
#[test_case("/{handle}/sheet-only?design"; "a schematic alone asked to stay")]
#[tokio::test]
async fn a_cacheable_page_renders_the_same_bytes_with_a_session_as_without(template: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let mut site = Site::up(db).await;
    let path = site.url(template);
    // The home page lists every test's designs, and the other cases push
    // while this one runs. Two anonymous reads either side of the session's
    // show the page held still; only then does a difference mean the
    // session.
    for _ in 0..100 {
        let before = http::request(site.addr, "GET", &path, &[], "").await;
        let with_session = site.owner.get(&path).await;
        let after = http::request(site.addr, "GET", &path, &[], "").await;
        if comparable(&before) != comparable(&after) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            continue;
        }
        assert_eq!(
            comparable(&before),
            comparable(&with_session),
            "{path} depends on the session"
        );
        assert!(
            with_session.header("set-cookie").is_none(),
            "{path} touched the session cookie"
        );
        return;
    }
    panic!("{path} never held still between two reads");
}

/// The server over `db` with a purge recorder.
async fn recording_server(db: PgPool) -> (SocketAddr, Arc<Recorder>, TempDir) {
    let dir = scratch_dir!();
    let purge = Arc::new(Recorder::default());
    let state = http::state_with_purge(db, Arc::new(FsBlobStore::new(dir.path())), purge.clone());
    (http::serve_on_free_port(state).await, purge, dir)
}

/// The design the browser's person just made.
async fn only_design(db: &PgPool, owner: AccountId) -> Design {
    design::by_owner(db, owner)
        .await
        .expect("the owner's designs")
        .remove(0)
}

/// A response's caching: its `Cache-Control` and `Surrogate-Key`.
fn caching(response: &http::Response) -> (Option<&str>, Option<&str>) {
    (
        response.header("cache-control"),
        response.header("surrogate-key"),
    )
}

#[test_case("/", 200, Policy::Page, Some("home"); "the home page")]
#[test_case("/{handle}", 200, Policy::Page, Some("handle-{handle}"); "a person")]
#[test_case("{path}", 200, Policy::Page, Some("{design_key}"); "a design")]
#[test_case("{path}/b/main", 200, Policy::Page, Some("{design_key}"); "a branch")]
#[test_case("{path}/r/{revision}", 200, Policy::Page, Some("{design_key}"); "a revision")]
#[test_case("{path}/b/main/history", 200, Policy::Page, Some("{design_key}"); "a history")]
#[test_case("{path}/branches", 200, Policy::Page, Some("{design_key}"); "the branches page")]
#[test_case("/{handle}/no-such-design/branches", 404, Policy::NotFound, Some("handle-{handle}"); "no design's branches")]
#[test_case("{path}/schematic", 200, Policy::Page, Some("{design_key}"); "a schematic")]
#[test_case("/about", 200, Policy::Page, None; "the about page")]
#[test_case("/viewer", 200, Policy::Page, None; "the hand-off viewer")]
#[test_case("/schematic", 200, Policy::Page, None; "the hand-off schematic")]
#[test_case("/{handle}/no-such-design", 404, Policy::NotFound, Some("handle-{handle}"); "no design")]
#[test_case("/{nobody}", 404, Policy::NotFound, Some("handle-{nobody}"); "no person")]
#[test_case("{path}/b/no-such-branch", 404, Policy::NotFound, Some("handle-{handle}"); "no branch")]
#[test_case("{path}/r/not-a-hash", 404, Policy::NotFound, Some("handle-{handle}"); "no revision")]
#[test_case("{path}/b/not..a@name/schematic", 404, Policy::NotFound, Some("handle-{handle}"); "no branch's schematic")]
#[test_case("{path}/r/not-a-hash/schematic", 404, Policy::NotFound, Some("handle-{handle}"); "no revision's schematic")]
#[test_case("/blob/0000000000000000000000000000000000000000000000000000000000000000", 404, Policy::NotFound, None; "no blob")]
#[test_case("/a/path/no/route/has/at/all", 404, Policy::NotFound, None; "no route")]
#[test_case("/blob/{board}", 200, Policy::Immutable, None; "a blob")]
#[test_case("/upload", 200, Policy::Personal, None; "the upload form")]
#[test_case("/settings", 200, Policy::Personal, None; "the settings page")]
#[test_case("/settings/sessions", 200, Policy::Personal, None; "the sessions page")]
#[test_case("/api/me", 200, Policy::Personal, None; "who is signed in")]
#[test_case("{path}/people", 200, Policy::Personal, None; "the people page")]
#[test_case("{path}/branches/allowed", 200, Policy::Personal, None; "what the viewer may do")]
#[test_case("/healthz", 200, Policy::Personal, None; "the health route")]
#[tokio::test]
async fn a_route_names_its_policy_and_its_keys(
    template: &str,
    status: u16,
    policy: Policy,
    key: Option<&str>,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let mut site = Site::up(db).await;
    let url = site.url(template);
    let expected = key.map(|key| site.url(key));
    let response = site.owner.get(&url).await;
    assert_eq!(response.status, status, "{url}: {}", response.body);
    assert_eq!(
        caching(&response),
        (Some(policy.value()), expected.as_deref()),
        "{url}"
    );
}

#[test_case("{path}/delete", "confirm=not-the-name"; "a design's delete")]
#[test_case("/settings/delete", "confirm=not-the-handle"; "an account's delete")]
#[tokio::test]
async fn a_delete_form_answers_for_the_person_alone(template: &str, body: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let mut site = Site::up(db).await;
    let url = site.url(template);
    let refused = site.owner.post_form(&url, body).await;
    assert_eq!(refused.status, 422, "{url}: {}", refused.body);
    assert_eq!(
        caching(&refused),
        (Some(Policy::Personal.value()), None),
        "{url}"
    );
}

#[tokio::test]
async fn a_slug_redirect_is_cached_under_the_handle_and_a_schematic_redirect_under_the_design() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let moved = owner
        .post_form(
            &format!("{path}/settings"),
            "name=Moved&slug=moved&description=",
        )
        .await;
    assert_eq!(moved.status, 303, "{}", moved.body);
    let redirect = http::request(addr, "GET", &path, &[], "").await;
    assert_eq!(redirect.status, 301);
    let on_handle = Key::Handle(handle.clone()).to_string();
    assert_eq!(
        caching(&redirect),
        (Some(Policy::Page.value()), Some(on_handle.as_str())),
        "a new design taking the slug is a push, which purges the handle"
    );

    let posted = owner
        .post_multipart(
            "/upload",
            &[
                Part::text("name", "sheet only"),
                Part::file("sheet.kicad_sch", SCHEMATIC),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    let sheet_only = design::by_owner(&db, owner_id)
        .await
        .unwrap()
        .into_iter()
        .find(|design| design.slug.as_str() == "sheet-only")
        .expect("the design");
    let opens = http::request(addr, "GET", &format!("/{handle}/sheet-only"), &[], "").await;
    assert_eq!(opens.status, 303);
    let on_design = Key::Design(sheet_only.id).to_string();
    assert_eq!(
        caching(&opens),
        (Some(Policy::Page.value()), Some(on_design.as_str()))
    );
}

#[tokio::test]
async fn a_blocked_blob_is_kept_briefly_so_a_cleared_takedown_comes_back() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, _, _) = signed_in(&db, addr).await;
    let bytes = format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("sch"));
    let hash = BlobHash::of(bytes.as_bytes());
    let posted = owner
        .post_multipart(
            "/upload",
            &[Part::file("blocked.kicad_sch", bytes.as_bytes())],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
    sqlx::query("INSERT INTO blocked_blob (hash, reason) VALUES ($1, 'test')")
        .bind(hash.as_bytes())
        .execute(&db)
        .await
        .unwrap();
    let blocked = http::request(addr, "GET", &format!("/blob/{hash}"), &[], "").await;
    assert_eq!(blocked.status, 410);
    assert_eq!(
        caching(&blocked),
        (Some(Policy::NotFound.value()), None),
        "never immutable: the edge asks again within a minute"
    );
    sqlx::query("DELETE FROM blocked_blob WHERE hash = $1")
        .bind(hash.as_bytes())
        .execute(&db)
        .await
        .unwrap();
    let cleared = http::request(addr, "GET", &format!("/blob/{hash}"), &[], "").await;
    assert_eq!(
        cleared.status, 200,
        "the takedown cleared, the blob is back"
    );
}

#[tokio::test]
async fn a_push_purges_the_design_the_owner_and_the_home_page_only_while_public() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    assert_eq!(
        purge.calls(),
        [vec![
            Key::Design(design.id),
            Key::Handle(handle.clone()),
            Key::Home
        ]],
        "the first push makes the design, and clears a 404 under the handle"
    );

    purge.clear();
    let refused = owner
        .post_multipart("/upload", &[Part::file("notes.txt", b"not kicad")])
        .await;
    assert_ne!(refused.status, 303, "{}", refused.body);
    assert!(purge.calls().is_empty(), "a refused push changes nothing");

    roles::set_visibility(&db, design.id, Visibility::Unlisted)
        .await
        .unwrap();
    let pushed = owner
        .post_multipart(
            "/upload",
            &[
                Part::text("design", path.trim_start_matches('/')),
                Part::file("midi8knob.kicad_pcb", BOARD),
                Part::file("midi8knob.kicad_sch", SCHEMATIC),
                Part::file("extra.kicad_pro", b"{}"),
            ],
        )
        .await;
    assert_eq!(pushed.status, 303, "{}", pushed.body);
    assert_eq!(
        purge.calls(),
        [vec![Key::Design(design.id), Key::Handle(handle)]],
        "an unlisted design is on no home page"
    );
}

#[tokio::test]
async fn a_cached_404_under_a_handle_is_cleared_by_the_push_that_makes_the_design() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, _, handle) = signed_in(&db, addr).await;
    let before = http::request(addr, "GET", &format!("/{handle}/midi8knob"), &[], "").await;
    assert_eq!(before.status, 404);
    let held = before.header("surrogate-key").expect("the 404's key");
    uploaded(&mut owner).await;
    assert!(
        purge.keys().iter().any(|key| key.to_string() == held),
        "the push purged {held}: {:?}",
        purge.keys()
    );
}

#[tokio::test]
async fn the_settings_form_purges_the_design_and_its_listings() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    purge.clear();
    let refused = owner
        .post_form(
            &format!("{path}/settings"),
            "name=&slug=midi8knob&description=",
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(purge.calls().is_empty(), "a refused save changes nothing");
    let saved = owner
        .post_form(
            &format!("{path}/settings"),
            "name=Renamed&slug=renamed&description=words",
        )
        .await;
    assert_eq!(saved.status, 303, "{}", saved.body);
    assert_eq!(
        purge.calls(),
        [vec![Key::Design(design.id), Key::Handle(handle), Key::Home]]
    );
}

#[tokio::test]
async fn every_branch_change_purges_the_design_and_its_listings_and_a_refused_one_nothing() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    let head = kiewer_lib::branch::list(&db, design.id).await.unwrap()[0].head;
    let new_branch = format!("name=side&from={head}");
    let posted = [
        (format!("{path}/branches"), new_branch.as_str()),
        (format!("{path}/b/side/rename"), "name=other"),
        (format!("{path}/b/other/default"), ""),
        (format!("{path}/b/main/delete"), ""),
    ];
    let keys = vec![
        Key::Design(design.id),
        Key::Handle(handle.clone()),
        Key::Home,
    ];
    for (url, body) in posted {
        purge.clear();
        let answer = owner.post_form(&url, body).await;
        assert_eq!(answer.status, 303, "{url}: {}", answer.body);
        assert_eq!(
            answer.header("cache-control"),
            Some(Policy::Personal.value())
        );
        assert_eq!(purge.calls(), std::slice::from_ref(&keys), "{url}");
    }
    purge.clear();
    let refused = owner.post_form(&format!("{path}/b/other/delete"), "").await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(
        refused.header("cache-control"),
        Some(Policy::Personal.value())
    );
    assert!(purge.calls().is_empty(), "a refused change changes nothing");
}

#[tokio::test]
async fn a_held_upload_purges_nothing_and_its_decision_purges_what_a_push_does() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    let base = kiewer_lib::branch::list(&db, design.id).await.unwrap()[0]
        .head
        .to_string();
    let design_path = path.trim_start_matches('/');
    let seen_on = format!("{design_path}/main");
    let fields = [
        Part::text("design", design_path),
        Part::text("branch", "main"),
        Part::text("from", &base),
        Part::text("seen_on", &seen_on),
        Part::file("midi8knob.kicad_pcb", BOARD),
    ];
    let landed = owner.post_multipart("/upload", &fields).await;
    assert_eq!(landed.status, 303, "{}", landed.body);
    purge.clear();
    let held = owner.post_multipart("/upload", &fields).await;
    assert_eq!(held.status, 303, "{}", held.body);
    assert_eq!(held.header("cache-control"), Some(Policy::Personal.value()));
    assert!(purge.calls().is_empty(), "no page shows a held upload");
    let choice = held.header("location").expect("the choice").to_owned();
    let page = owner.get(&choice).await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(caching(&page), (Some(Policy::Personal.value()), None));
    let new_head = kiewer_lib::branch::list(&db, design.id).await.unwrap()[0].head;
    let decided = owner
        .post_form(&format!("{choice}/reappend"), &format!("onto={new_head}"))
        .await;
    assert_eq!(decided.status, 303, "{}", decided.body);
    assert_eq!(
        purge.calls(),
        [vec![Key::Design(design.id), Key::Handle(handle), Key::Home]]
    );
    let gone = owner.get(&choice).await;
    assert_eq!(gone.status, 404);
    assert_eq!(caching(&gone), (Some(Policy::Personal.value()), None));
}

#[test_case("unlisted"; "to unlisted")]
#[test_case("public"; "to public")]
#[tokio::test]
async fn a_visibility_change_purges_all_three_whichever_way_it_goes(visibility: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    purge.clear();
    let set = owner
        .post_form(
            &format!("{path}/people/visibility"),
            &format!("visibility={visibility}"),
        )
        .await;
    assert_eq!(set.status, 303, "{}", set.body);
    assert_eq!(
        purge.calls(),
        [vec![Key::Design(design.id), Key::Handle(handle), Key::Home]]
    );
}

#[tokio::test]
async fn a_refused_visibility_purges_nothing() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, _, _) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    purge.clear();
    let refused = owner
        .post_form(&format!("{path}/people/visibility"), "visibility=secret")
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(purge.calls().is_empty(), "{:?}", purge.calls());
}

#[tokio::test]
async fn choosing_a_handle_clears_a_cached_404_at_that_handle() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let signed_in = account::sign_in(
        &db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "newcomer".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let id = session::start(&db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    let handle = fresh("n");
    let before = http::request(addr, "GET", &format!("/{handle}"), &[], "").await;
    assert_eq!(before.status, 404);
    let held = before.header("surrogate-key").expect("the 404's key");
    let chosen = browser
        .post_form("/welcome", &format!("handle={handle}"))
        .await;
    assert_eq!(chosen.status, 303, "{}", chosen.body);
    assert_eq!(
        purge
            .keys()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [held]
    );
}

#[tokio::test]
async fn a_transfer_purges_both_handles_and_a_refused_one_nothing() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let design = only_design(&db, owner_id).await;
    let (_, _, heir) = signed_in(&db, addr).await;
    purge.clear();
    let nobody = owner
        .post_form(&format!("{path}/people/transfer"), "handle=nobody-at-all")
        .await;
    assert_eq!(nobody.status, 404, "{}", nobody.body);
    assert!(purge.calls().is_empty(), "nothing moved");
    let moved = owner
        .post_form(
            &format!("{path}/people/transfer"),
            &format!("handle={heir}"),
        )
        .await;
    assert_eq!(moved.status, 303, "{}", moved.body);
    assert_eq!(
        purge.calls(),
        [vec![
            Key::Design(design.id),
            Key::Handle(handle),
            Key::Handle(heir),
            Key::Home
        ]]
    );
}

#[tokio::test]
async fn a_role_change_purges_nothing_since_no_cached_page_shows_roles() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, _, _) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let (_, _, helper) = signed_in(&db, addr).await;
    purge.clear();
    let before = http::request(addr, "GET", &path, &[], "").await;
    let added = owner
        .post_form(
            &format!("{path}/people/role"),
            &format!("handle={helper}&role=admin"),
        )
        .await;
    assert_eq!(added.status, 303, "{}", added.body);
    assert!(purge.calls().is_empty(), "{:?}", purge.calls());
    let after = http::request(addr, "GET", &path, &[], "").await;
    assert_eq!(after.body, before.body, "the page is what it was");
}

#[tokio::test]
async fn a_fork_purges_the_forkers_page_and_leaves_the_source_alone() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = recording_server(db.clone()).await;
    let (mut owner, owner_id, _) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let source = only_design(&db, owner_id).await;
    let (mut forker, forker_id, forker_handle) = signed_in(&db, addr).await;
    purge.clear();
    let made = forker
        .post_form(&format!("{path}/fork"), "take=default")
        .await;
    assert_eq!(made.status, 303, "{}", made.body);
    let fork = only_design(&db, forker_id).await;
    assert_eq!(
        purge.calls(),
        [vec![
            Key::Design(fork.id),
            Key::Handle(forker_handle.clone()),
            Key::Home
        ]],
        "the source's pages do not change: {:?}",
        Key::Design(source.id)
    );

    // The fork's pages show the source's handle and slug, so they carry
    // its key; the history page shows neither.
    let page = http::request(addr, "GET", &format!("/{forker_handle}/midi8knob"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        page.header("surrogate-key").map(ToOwned::to_owned),
        Some(surrogate_key(&[
            Key::Design(fork.id),
            Key::Design(source.id)
        ]))
    );
    purge.clear();
    let moved = owner
        .post_form(
            &format!("{path}/settings"),
            "name=Moved&slug=moved&description=",
        )
        .await;
    assert_eq!(moved.status, 303, "{}", moved.body);
    assert!(
        purge.keys().contains(&Key::Design(source.id)),
        "a source's slug change reaches the fork's page: {:?}",
        purge.keys()
    );
}

/// A purge that always fails: the edge is down.
#[derive(Debug)]
struct Unreachable;

#[async_trait]
impl Purge for Unreachable {
    async fn purge(&self, _keys: &[Key]) -> kiewer_lib::Result<()> {
        Err(kiewer_lib::Error::Failed {
            reason: "the edge is down".to_owned(),
        })
    }
}

#[tokio::test]
async fn a_failed_purge_never_fails_the_write() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let state = http::state_with_purge(
        db.clone(),
        Arc::new(FsBlobStore::new(dir.path())),
        Arc::new(Unreachable),
    );
    let addr = http::serve_on_free_port(state).await;
    let (mut owner, _, _) = signed_in(&db, addr).await;
    let path = uploaded(&mut owner).await;
    let saved = owner
        .post_form(
            &format!("{path}/settings"),
            "name=Still+saved&slug=midi8knob&description=",
        )
        .await;
    assert_eq!(saved.status, 303, "{}", saved.body);
    let page = http::request(addr, "GET", &path, &[], "").await;
    assert!(page.body.contains("Still saved"), "{}", page.body);
}
