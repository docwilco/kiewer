//! Roles and visibility (#21): who may write what on a design, through
//! `kiewer_lib::roles` and over HTTP. Gated on `DATABASE_URL` like
//! `tests/db.rs`; rows persist between runs on a dev box, so every test
//! picks its own handles.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::Error;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Auth, Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::design::{self, Design};
use kiewer_lib::roles::{self, Role, Visibility};
use kiewer_lib::session;
use kiewer_lib::web::AppState;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

/// A person with a handle.
async fn person(db: &PgPool) -> (AccountId, Handle) {
    let signed_in = account::sign_in(
        db,
        Provider::GitHub,
        &ProviderUser {
            id: fresh("gh"),
            login: "someone".to_owned(),
            email: None,
        },
    )
    .await
    .expect("sign in");
    let handle: Handle = fresh("p").parse().expect("a handle");
    account::choose_handle(db, signed_in.account_id, &handle)
        .await
        .expect("choose");
    (signed_in.account_id, handle)
}

/// A new design of `owner` at `slug`, committed.
async fn new_design(db: &PgPool, owner: AccountId, slug: &str) -> Design {
    let mut tx = db.begin().await.unwrap();
    let design = design::create(&mut tx, owner, &slug.parse().unwrap(), "A design")
        .await
        .expect("a new design");
    tx.commit().await.unwrap();
    design
}

#[tokio::test]
async fn the_owner_is_the_designs_owner_column_and_a_stranger_has_no_role() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let (stranger, _) = person(&db).await;
    let design = new_design(&db, owner, "mine").await;
    assert_eq!(
        design.visibility,
        Visibility::Public,
        "public until changed"
    );
    assert_eq!(
        roles::role_of(&db, design.id, owner).await.unwrap(),
        Some(Role::Owner)
    );
    assert_eq!(
        roles::role_of(&db, design.id, stranger).await.unwrap(),
        None
    );
    assert_eq!(
        roles::require(&db, design.id, owner, Role::Owner)
            .await
            .unwrap(),
        Role::Owner
    );
    let refused = roles::require(&db, design.id, stranger, Role::Collaborator)
        .await
        .expect_err("a stranger may not push");
    assert!(
        matches!(
            refused,
            Error::Forbidden {
                needed: Role::Collaborator
            }
        ),
        "{refused}"
    );
    assert!(refused.to_string().contains("collaborator"), "{refused}");
}

#[test_case(Role::Collaborator, Role::Collaborator, true; "a collaborator may do a collaborator's write")]
#[test_case(Role::Collaborator, Role::Admin, false; "a collaborator may not do an admin's")]
#[test_case(Role::Collaborator, Role::Owner, false; "a collaborator may not do the owner's")]
#[test_case(Role::Admin, Role::Collaborator, true; "an admin may do a collaborator's")]
#[test_case(Role::Admin, Role::Admin, true; "an admin may do an admin's")]
#[test_case(Role::Admin, Role::Owner, false; "an admin may not do the owner's")]
#[tokio::test]
async fn a_granted_role_is_checked_against_what_a_write_needs(
    granted: Role,
    needed: Role,
    allowed: bool,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (owner, _) = person(&db).await;
    let (member, member_handle) = person(&db).await;
    let design = new_design(&db, owner, "shared").await;
    assert_eq!(
        roles::set_role(&db, design.id, &member_handle, granted)
            .await
            .unwrap(),
        roles::Changed::Done
    );
    assert_eq!(
        roles::role_of(&db, design.id, member).await.unwrap(),
        Some(granted)
    );
    let checked = roles::require(&db, design.id, member, needed).await;
    match checked {
        Ok(role) => assert!(allowed && role == granted, "{role}"),
        Err(error) => assert!(
            !allowed && matches!(error, Error::Forbidden { needed: n } if n == needed),
            "{error}"
        ),
    }
}

/// The server over `db` with a filesystem blob store under cargo's
/// scratch directory and no sign-in provider.
async fn server(db: PgPool) -> (SocketAddr, TempDir) {
    let dir = scratch_dir!();
    let state = AppState {
        db,
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth")),
    };
    (http::serve_on_free_port(state).await, dir)
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    let (account_id, handle) = person(db).await;
    let id = session::start(db, account_id).await.expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, account_id, handle)
}

/// A design of `owner` with one revision, pushed through the upload
/// route, at `/{owner}/{slug}`.
async fn uploaded(browser: &mut http::Browser, slug: &str) {
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", slug),
                Part::file(
                    "demo.kicad_sch",
                    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).as_bytes(),
                ),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
}

/// A push of one sheet onto the design at `path`.
async fn push_to(browser: &mut http::Browser, path: &str) -> http::Response {
    browser
        .post_multipart(
            "/upload",
            &[
                Part::text("design", path),
                Part::text("message", "another sheet"),
                Part::file(
                    "demo.kicad_sch",
                    format!("(kicad_sch (version 20250114) (uuid \"{}\"))", fresh("s")).as_bytes(),
                ),
            ],
        )
        .await
}

#[test_case(None, 403; "a stranger may not push")]
#[test_case(Some(Role::Collaborator), 303; "a collaborator pushes")]
#[test_case(Some(Role::Admin), 303; "an admin pushes")]
#[tokio::test]
async fn a_push_to_someone_elses_design_needs_the_collaborator_role(
    granted: Option<Role>,
    status: u16,
) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, owner_handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "shared").await;
    let path = format!("{owner_handle}/shared");
    let design = design::by_owner(&db, owner_id).await.unwrap().remove(0);

    let (mut member, _, member_handle) = signed_in(&db, addr).await;
    if let Some(role) = granted {
        roles::set_role(&db, design.id, &member_handle, role)
            .await
            .unwrap();
    }
    let form = member.get("/upload").await;
    assert_eq!(
        form.body.contains(&format!(r#"<option value="{path}">"#)),
        granted.is_some(),
        "the form offers the designs the person may push to: {}",
        form.body
    );
    let pushed = push_to(&mut member, &path).await;
    assert_eq!(pushed.status, status, "{}", pushed.body);
    if status == 303 {
        assert_eq!(pushed.header("location"), Some(format!("/{path}").as_str()));
        let page = http::request(addr, "GET", &format!("/{path}"), &[], "").await;
        assert!(page.body.contains("another sheet"), "{}", page.body);
        assert!(
            page.body.contains(member_handle.as_str()),
            "the member is the author: {}",
            page.body
        );
    } else {
        assert!(pushed.body.contains("collaborator"), "{}", pushed.body);
        assert_eq!(pushed.header("cache-control"), Some("private, no-store"));
    }
    let own = push_to(&mut owner, &path).await;
    assert_eq!(own.status, 303, "the owner always pushes: {}", own.body);
    let missing = push_to(&mut owner, &format!("{owner_handle}/nothing")).await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert!(missing.body.contains("no design"), "{}", missing.body);
}
