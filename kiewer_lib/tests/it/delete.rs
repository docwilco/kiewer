//! Deleting a design and an account over HTTP (#23, #86): the owner
//! alone deletes a design, with its name typed out; a person deletes
//! their own account, with their handle typed out, and its designs,
//! roles and sessions go with it; the handle stays out of reach for 90
//! days; the keys of what went are purged. The Postgres tests skip
//! without `DATABASE_URL`, like `tests/it/db.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kiewer_assert::assert_is_empty;
use kiewer_lib::Error;
use kiewer_lib::account::{self, AccountId, Handle};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::cache::{Key, Recorder};
use kiewer_lib::design::{self, Design};
use kiewer_lib::roles::{self, Role, Visibility};
use kiewer_lib::session;
use kiewer_test_support::db::{fresh, pool_from_env};
use kiewer_test_support::http::{self, Part};
use kiewer_test_support::{TempDir, scratch_dir};
use sqlx::PgPool;
use test_case::test_case;

/// The server over `db`, with a purge recorder.
async fn server(db: PgPool) -> (SocketAddr, Arc<Recorder>, TempDir) {
    let dir = scratch_dir!();
    let purge = Arc::new(Recorder::default());
    let state = http::state_with_purge(db, Arc::new(FsBlobStore::new(dir.path())), purge.clone());
    (http::serve_on_free_port(state).await, purge, dir)
}

/// A provider user nobody has signed in as.
fn provider_user() -> ProviderUser {
    ProviderUser {
        id: fresh("gh"),
        login: "leaver".to_owned(),
        email: None,
    }
}

/// A person with a handle and a live session: the browser carries the
/// cookie as if they had signed in.
async fn person(
    db: &PgPool,
    addr: SocketAddr,
    user: &ProviderUser,
) -> (http::Browser, AccountId, Handle) {
    let signed_in = account::sign_in(db, Provider::GitHub, user)
        .await
        .expect("sign in");
    let handle: Handle = fresh("d").parse().expect("a handle");
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

async fn signed_in(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId, Handle) {
    person(db, addr, &provider_user()).await
}

/// A board pushed as a new design named `name`, of the browser's person.
async fn uploaded(browser: &mut http::Browser, name: &str) {
    let bytes = format!(
        "(kicad_pcb (version 20241229) (generator \"{}\") (layers (0 \"F.Cu\" signal)))",
        fresh("board")
    );
    let posted = browser
        .post_multipart(
            "/upload",
            &[
                Part::text("name", name),
                Part::file("board.kicad_pcb", bytes.as_bytes()),
            ],
        )
        .await;
    assert_eq!(posted.status, 303, "{}", posted.body);
}

/// The design at `slug` of `owner`.
async fn design_of(db: &PgPool, owner: AccountId, slug: &str) -> Design {
    design::by_owner(db, owner)
        .await
        .expect("the owner's designs")
        .into_iter()
        .find(|design| design.slug.as_str() == slug)
        .expect("the design")
}

async fn design_count(db: &PgPool, owner: AccountId) -> usize {
    design::by_owner(db, owner)
        .await
        .expect("the owner's designs")
        .len()
}

async fn delete_design(
    browser: &mut http::Browser,
    handle: &Handle,
    slug: &str,
    typed: &str,
) -> http::Response {
    browser
        .post_form(
            &format!("/{handle}/{slug}/delete"),
            &format!("confirm={typed}"),
        )
        .await
}

#[tokio::test]
async fn the_owner_deletes_a_design_and_its_keys_are_purged() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "Doomed Board").await;
    let doomed = design_of(&db, owner_id, "doomed-board").await;
    purge.clear();

    let deleted = delete_design(&mut owner, &handle, "doomed-board", "Doomed%20Board").await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    assert_eq!(
        deleted.header("location"),
        Some(format!("/{handle}").as_str())
    );
    assert_eq!(deleted.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        purge.calls(),
        [vec![
            Key::Design(doomed.id),
            Key::Handle(handle.clone()),
            Key::Home
        ]],
        "a public design leaves the home page too"
    );

    let (status, _) = http::get(addr, &format!("/{handle}/doomed-board")).await;
    assert_eq!(status, 404, "the page is gone");
    let branches: i64 = sqlx::query_scalar("SELECT count(*) FROM branch WHERE design_id = $1")
        .bind(doomed.id)
        .fetch_one(&db)
        .await
        .expect("a read");
    assert_eq!(branches, 0, "its branches went with it");
    let again = delete_design(&mut owner, &handle, "doomed-board", "Doomed%20Board").await;
    assert_eq!(again.status, 404, "{}", again.body);
}

#[tokio::test]
async fn an_unlisted_design_leaves_the_home_page_alone() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "quiet").await;
    let quiet = design_of(&db, owner_id, "quiet").await;
    roles::set_visibility(&db, quiet.id, Visibility::Unlisted)
        .await
        .expect("unlisted");
    purge.clear();
    let deleted = delete_design(&mut owner, &handle, "quiet", "quiet").await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    assert_eq!(
        purge.calls(),
        [vec![Key::Design(quiet.id), Key::Handle(handle)]]
    );
}

#[test_case(Some(Role::Collaborator); "a collaborator")]
#[test_case(Some(Role::Admin); "an admin")]
#[test_case(None; "a stranger")]
#[tokio::test]
async fn nobody_but_the_owner_deletes_a_design(role: Option<Role>) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, owner_handle) = signed_in(&db, addr).await;
    let (mut other, _, other_handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "kept").await;
    let kept = design_of(&db, owner_id, "kept").await;
    if let Some(role) = role {
        roles::set_role(&db, kept.id, &other_handle, role)
            .await
            .expect("a role");
    }
    purge.clear();

    let refused = delete_design(&mut other, &owner_handle, "kept", "kept").await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(refused.header("cache-control"), Some("private, no-store"));
    assert_eq!(design_count(&db, owner_id).await, 1, "it stands");
    assert!(purge.calls().is_empty(), "nothing changed, nothing purged");
}

#[tokio::test]
async fn a_signed_out_request_deletes_nothing() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "kept").await;
    let forged = http::request(
        addr,
        "POST",
        &format!("/{handle}/kept/delete"),
        &[],
        "confirm=kept",
    )
    .await;
    assert_eq!(forged.status, 303, "sent home: {}", forged.body);
    assert_eq!(forged.header("location"), Some("/"));
    assert_eq!(design_count(&db, owner_id).await, 1, "it stands");
    let forged = http::request(
        addr,
        "POST",
        "/settings/delete",
        &[],
        &format!("confirm={handle}"),
    )
    .await;
    assert_eq!(forged.status, 303, "sent home: {}", forged.body);
    assert_eq!(forged.header("location"), Some("/"));
    assert_eq!(design_count(&db, owner_id).await, 1, "it stands");
}

#[test_case("kept-board"; "the slug")]
#[test_case("Kept"; "part of the name")]
#[test_case(""; "nothing")]
#[tokio::test]
async fn the_wrong_name_deletes_nothing(typed: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "Kept Board").await;
    purge.clear();
    let refused = delete_design(&mut owner, &handle, "kept-board", typed).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(design_count(&db, owner_id).await, 1, "it stands");
    assert_is_empty!(purge.calls());
}

#[tokio::test]
async fn the_design_page_offers_the_delete_with_the_name_to_type() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let (mut owner, _, handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "On Show").await;
    let page = http::request(addr, "GET", &format!("/{handle}/on-show"), &[], "").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body
            .contains(&format!(r#"action="/{handle}/on-show/delete""#)),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(r#"data-name="On Show""#),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(r#"<button type="submit" disabled>"#),
        "the button starts off"
    );
}

async fn sessions_of(db: &PgPool, account: AccountId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM session WHERE account_id = $1")
        .bind(account)
        .fetch_one(db)
        .await
        .expect("a read")
}

#[tokio::test]
async fn deleting_an_account_takes_its_designs_roles_and_sessions_and_reserves_the_handle() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let user = provider_user();
    let (mut leaver, leaver_id, leaver_handle) = person(&db, addr, &user).await;
    let (mut other, other_id, other_handle) = signed_in(&db, addr).await;

    uploaded(&mut leaver, "Theirs").await;
    uploaded(&mut leaver, "Quiet").await;
    uploaded(&mut other, "Others").await;
    let theirs = design_of(&db, leaver_id, "theirs").await;
    let quiet = design_of(&db, leaver_id, "quiet").await;
    roles::set_visibility(&db, quiet.id, Visibility::Unlisted)
        .await
        .expect("unlisted");
    let others = design_of(&db, other_id, "others").await;
    roles::set_role(&db, others.id, &leaver_handle, Role::Collaborator)
        .await
        .expect("a role");
    let authored = other
        .post_form(&format!("/{leaver_handle}/theirs/fork"), "take=default")
        .await;
    assert_eq!(authored.status, 303, "{}", authored.body);
    let fork_slug = authored
        .header("location")
        .and_then(|location| location.rsplit('/').next())
        .expect("the fork's page")
        .to_owned();
    let second_session = session::start(&db, leaver_id).await.expect("a session");
    purge.clear();

    let deleted = leaver
        .post_form("/settings/delete", &format!("confirm={leaver_handle}"))
        .await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    assert_eq!(deleted.header("location"), Some("/"));
    assert_eq!(deleted.header("cache-control"), Some("private, no-store"));
    assert!(
        leaver.cookie("kiewer_session").is_none(),
        "the browser forgot the session"
    );

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE id = $1")
        .bind(leaver_id)
        .fetch_one(&db)
        .await
        .expect("a read");
    assert_eq!(accounts, 0, "the account is gone");
    assert_eq!(design_count(&db, leaver_id).await, 0, "its designs too");
    assert_eq!(sessions_of(&db, leaver_id).await, 0, "every session too");
    assert!(
        session::find(&db, &second_session)
            .await
            .expect("a read")
            .is_none(),
        "the other browser is signed out"
    );
    let role: Option<Role> = roles::role_of(&db, others.id, leaver_id)
        .await
        .expect("a read");
    assert_eq!(role, None, "its roles too");

    let (status, _) = http::get(addr, &format!("/{leaver_handle}")).await;
    assert_eq!(status, 404, "the person's page is gone");
    let (status, _) = http::get(addr, &format!("/{leaver_handle}/theirs")).await;
    assert_eq!(status, 404, "the design's page is gone");
    let fork = http::request(
        addr,
        "GET",
        &format!("/{other_handle}/{fork_slug}"),
        &[],
        "",
    )
    .await;
    assert_eq!(fork.status, 200, "another person's fork stands");
    assert!(
        fork.body
            .contains(&format!("Forked from {leaver_handle}/theirs (deleted)")),
        "{}",
        fork.body
    );

    let mut purged = purge.keys();
    purged.sort();
    let mut expected = vec![
        Key::Home,
        Key::Design(theirs.id),
        Key::Design(quiet.id),
        Key::Handle(leaver_handle.clone()),
    ];
    expected.sort();
    assert_eq!(purged, expected);

    // A new person, even the same one, gets another account and cannot
    // have the handle back.
    let again = account::sign_in(&db, Provider::GitHub, &user)
        .await
        .expect("sign in");
    assert!(again.is_new, "the same provider makes a fresh account");
    assert_ne!(again.account_id, leaver_id);
    let refused = account::choose_handle(&db, again.account_id, &leaver_handle).await;
    assert!(
        matches!(refused, Err(Error::HandleReserved(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_revision_outlives_its_authors_account_under_the_handle_as_text() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let (mut owner, owner_id, owner_handle) = signed_in(&db, addr).await;
    let (mut pusher, _, pusher_handle) = signed_in(&db, addr).await;
    uploaded(&mut owner, "together").await;
    let together = design_of(&db, owner_id, "together").await;
    roles::set_role(&db, together.id, &pusher_handle, Role::Collaborator)
        .await
        .expect("a role");
    let bytes = format!(
        "(kicad_pcb (version 20241229) (generator \"{}\") (layers (0 \"F.Cu\" signal)))",
        fresh("board")
    );
    let path = format!("{owner_handle}/together");
    let upload = pusher
        .post_multipart(
            "/upload",
            &[
                Part::text("design", &path),
                Part::text("branch", "main"),
                Part::file("board.kicad_pcb", bytes.as_bytes()),
            ],
        )
        .await;
    assert_eq!(upload.status, 303, "{}", upload.body);

    let deleted = pusher
        .post_form("/settings/delete", &format!("confirm={pusher_handle}"))
        .await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    let history = http::request(
        addr,
        "GET",
        &format!("/{owner_handle}/together/b/main/history"),
        &[],
        "",
    )
    .await;
    assert_eq!(history.status, 200, "{}", history.body);
    assert!(
        history.body.contains(pusher_handle.as_str()),
        "{}",
        history.body
    );
}

#[test_case("someone-else"; "another handle")]
#[test_case(""; "nothing")]
#[tokio::test]
async fn the_wrong_handle_deletes_nothing(typed: &str) {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, purge, _dir) = server(db.clone()).await;
    let (mut leaver, leaver_id, _) = signed_in(&db, addr).await;
    uploaded(&mut leaver, "kept").await;
    purge.clear();
    let refused = leaver
        .post_form("/settings/delete", &format!("confirm={typed}"))
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(design_count(&db, leaver_id).await, 1, "it stands");
    assert_eq!(sessions_of(&db, leaver_id).await, 1, "still signed in");
    assert_is_empty!(purge.calls());
    assert!(leaver.cookie("kiewer_session").is_some());
}

#[tokio::test]
async fn settings_says_what_goes_and_lists_the_designs_shared_with_others() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let (mut leaver, leaver_id, handle) = signed_in(&db, addr).await;
    let (_other, _, other_handle) = signed_in(&db, addr).await;
    uploaded(&mut leaver, "Alone Board").await;
    uploaded(&mut leaver, "Shared Board").await;
    let shared = design_of(&db, leaver_id, "shared-board").await;
    roles::set_role(&db, shared.id, &other_handle, Role::Admin)
        .await
        .expect("a role");

    let page = leaver.get("/settings").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(page.header("cache-control"), Some("private, no-store"));
    assert!(page.body.contains(r#"action="/settings/delete""#));
    assert!(
        page.body.contains(&format!(r#"pattern="{handle}""#)),
        "the handle typed out: {}",
        page.body
    );
    for word in [
        "the account",
        "all your designs",
        "your roles",
        "every session",
        "reserved for 90 days",
    ] {
        assert!(page.body.contains(word), "{word}: {}", page.body);
    }
    assert!(page.body.contains("<li>Shared Board</li>"), "{}", page.body);
    assert!(!page.body.contains("<li>Alone Board</li>"), "{}", page.body);
}

/// A signed-in person with no handle yet, as `/welcome` sees them.
async fn newcomer(db: &PgPool, addr: SocketAddr) -> (http::Browser, AccountId) {
    let signed_in = account::sign_in(db, Provider::GitHub, &provider_user())
        .await
        .expect("sign in");
    let id = session::start(db, signed_in.account_id)
        .await
        .expect("a session");
    let mut browser = http::Browser::new(addr);
    browser.set_cookie("kiewer_session", &id.to_string());
    (browser, signed_in.account_id)
}

/// The handle of a deleted account, deleted just now.
async fn given_up(db: &PgPool, addr: SocketAddr) -> Handle {
    let (mut leaver, _, handle) = signed_in(db, addr).await;
    let deleted = leaver
        .post_form("/settings/delete", &format!("confirm={handle}"))
        .await;
    assert_eq!(deleted.status, 303, "{}", deleted.body);
    handle
}

#[tokio::test]
async fn a_reserved_handle_is_refused_at_welcome_for_90_days() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let handle = given_up(&db, addr).await;
    let days: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM reserved_until - now())::float8 / 86400
         FROM reserved_handle WHERE handle = $1",
    )
    .bind(handle.as_str())
    .fetch_one(&db)
    .await
    .expect("the reservation");
    assert!((89.9..=90.0).contains(&days), "{days} days");

    let (mut browser, account_id) = newcomer(&db, addr).await;
    let refused = browser
        .post_form("/welcome", &format!("handle={handle}"))
        .await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(refused.body.contains("90 days"), "{}", refused.body);
    assert!(
        refused.body.contains(&format!(r#"value="{handle}""#)),
        "the form comes back with the attempt: {}",
        refused.body
    );
    let chosen: Option<String> = sqlx::query_scalar("SELECT handle FROM account WHERE id = $1")
        .bind(account_id)
        .fetch_one(&db)
        .await
        .expect("a read");
    assert_eq!(chosen, None, "the refusal took nothing");

    // Day 91: the reservation has run out.
    sqlx::query(
        "UPDATE reserved_handle SET reserved_until = now() - interval '1 second'
         WHERE handle = $1",
    )
    .bind(handle.as_str())
    .execute(&db)
    .await
    .expect("run out");
    let taken = browser
        .post_form("/welcome", &format!("handle={handle}"))
        .await;
    assert_eq!(taken.status, 303, "{}", taken.body);
}

/// The window the second statement of `choose_handle` exists for: the
/// delete is under way, the handle looks free to a read, and the update
/// has to wait for the delete's commit.
#[tokio::test]
async fn a_choice_racing_the_delete_sees_the_reservation() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let (addr, _purge, _dir) = server(db.clone()).await;
    let (_leaver, leaver_id, handle) = signed_in(&db, addr).await;
    let (_browser, chooser) = newcomer(&db, addr).await;

    let mut deleting = db.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO reserved_handle (handle, reserved_until)
         VALUES ($1, now() + interval '90 days')",
    )
    .bind(handle.as_str())
    .execute(&mut *deleting)
    .await
    .expect("a reservation");
    sqlx::query("DELETE FROM account WHERE id = $1")
        .bind(leaver_id)
        .execute(&mut *deleting)
        .await
        .expect("the delete");

    let choosing = {
        let (db, handle) = (db.clone(), handle.clone());
        tokio::spawn(async move { account::choose_handle(&db, chooser, &handle).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!choosing.is_finished(), "the update waits for the delete");
    deleting.commit().await.expect("commit");

    let refused = choosing.await.expect("the task");
    assert!(
        matches!(refused, Err(Error::HandleReserved(_))),
        "{refused:?}"
    );
}
