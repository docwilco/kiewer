//! Accounts, identities and sessions against Postgres: the three tables
//! and the session lifecycle through `kiewer_lib::account` and
//! `kiewer_lib::session`. Gated on `DATABASE_URL` like `tests/db.rs`.
//! Rows persist between runs on a dev box, so every test picks its own
//! provider user id and handle.

use kiewer_lib::Error;
use kiewer_lib::account::{self, Handle};
use kiewer_lib::auth::Provider;
use kiewer_lib::session::{self, SessionId};
use kiewer_test_support::db::{fresh, pool_from_env};
use sqlx::PgPool;

#[tokio::test]
async fn the_account_identity_and_session_tables_exist() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name IN ('account', 'identity', 'session')
         ORDER BY table_name",
    )
    .fetch_all(&db)
    .await
    .expect("information_schema answers");
    assert_eq!(tables, ["account", "identity", "session"]);
}

#[tokio::test]
async fn a_first_sign_in_makes_an_account_and_a_second_finds_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let user_id = fresh("gh");
    let first = account::sign_in(&db, Provider::GitHub, &user_id, "octocat")
        .await
        .expect("a first sign-in");
    assert!(first.is_new, "the first sign-in makes the account");
    assert_eq!(first.handle, None, "no handle until the person picks one");
    assert_eq!(first.provider_login, "octocat");
    let second = account::sign_in(&db, Provider::GitHub, &user_id, "octocat-renamed")
        .await
        .expect("a second sign-in");
    assert!(!second.is_new);
    assert_eq!(second.account_id, first.account_id);
    assert_eq!(
        second.provider_login, "octocat-renamed",
        "the login the provider shows now is the one kept"
    );
    // Two providers' identities are never merged by login or email.
    let other = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "octocat")
        .await
        .expect("another person with the same login");
    assert_ne!(other.account_id, first.account_id);
}

#[tokio::test]
async fn a_handle_is_chosen_once_and_a_taken_one_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let alice = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "alice")
        .await
        .expect("sign in");
    let bob = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "bob")
        .await
        .expect("sign in");
    let handle: Handle = fresh("alice").parse().expect("a valid handle");
    account::choose_handle(&db, alice.account_id, &handle)
        .await
        .expect("the first choice is free");
    let err = account::choose_handle(&db, bob.account_id, &handle)
        .await
        .expect_err("a taken handle");
    assert!(
        matches!(err, Error::HandleTaken(ref taken) if taken == &handle),
        "{err}"
    );
    let again = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "alice").await;
    assert!(again.is_ok(), "the refusal left the database usable");
    let other: Handle = fresh("alice").parse().expect("a valid handle");
    let err = account::choose_handle(&db, alice.account_id, &other)
        .await
        .expect_err("no rename in v1");
    assert!(!matches!(err, Error::HandleTaken(_)), "{err}");
}

#[tokio::test]
async fn a_session_starts_is_found_and_ends() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let signed_in = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "carol")
        .await
        .expect("sign in");
    let id = session::start(&db, signed_in.account_id)
        .await
        .expect("a session starts");
    // The cookie carries the id as text; the server parses it back.
    let parsed: SessionId = id.to_string().parse().expect("round-trips as text");
    assert_eq!(parsed, id);
    let found = session::find(&db, &id)
        .await
        .expect("lookup")
        .expect("a fresh session is found");
    assert_eq!(found.account_id, signed_in.account_id);
    assert_eq!(found.handle, None);
    assert!(!found.refreshed, "a fresh session has nothing to refresh");
    let handle: Handle = fresh("carol").parse().expect("valid");
    account::choose_handle(&db, signed_in.account_id, &handle)
        .await
        .expect("free");
    let found = session::find(&db, &id)
        .await
        .expect("lookup")
        .expect("found");
    assert_eq!(found.handle.as_ref(), Some(&handle));
    session::end(&db, &id).await.expect("sign out");
    assert!(
        session::find(&db, &id).await.expect("lookup").is_none(),
        "sign-out ends the session"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM session WHERE account_id = $1")
        .bind(signed_in.account_id)
        .fetch_one(&db)
        .await
        .expect("count");
    assert_eq!(rows, 0, "sign-out removes the row, not just its validity");
    session::end(&db, &id).await.expect("ending twice is fine");
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let id = SessionId::random();
    assert!(session::find(&db, &id).await.expect("lookup").is_none());
}

/// Sets a session's expiry from the test, in place of a clock: the
/// server's clock is Postgres's `now()`.
async fn age_session(db: &PgPool, id: &SessionId, remaining: &str) {
    let changed = sqlx::query(
        "UPDATE session SET expires_at = now() + $2::interval WHERE id_hash = sha256($1)",
    )
    .bind(id.to_string().into_bytes())
    .bind(remaining)
    .execute(db)
    .await
    .expect("update")
    .rows_affected();
    assert_eq!(changed, 1, "the session row is there to age");
}

#[tokio::test]
async fn a_session_slides_its_expiry_when_used_after_a_day() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let signed_in = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "dave")
        .await
        .expect("sign in");
    let id = session::start(&db, signed_in.account_id)
        .await
        .expect("start");
    age_session(&db, &id, "20 days").await;
    let found = session::find(&db, &id)
        .await
        .expect("lookup")
        .expect("found");
    assert!(found.refreshed, "used with 20 of 30 days left: refreshed");
    let remaining: f64 = sqlx::query_scalar(
        "SELECT (extract(epoch FROM expires_at - now()) / 86400)::float8 FROM session WHERE id_hash = sha256($1)",
    )
    .bind(id.to_string().into_bytes())
    .fetch_one(&db)
    .await
    .expect("days left");
    assert!(remaining > 29.9, "back to 30 days, was {remaining}");
    let found = session::find(&db, &id)
        .await
        .expect("lookup")
        .expect("found");
    assert!(!found.refreshed, "just refreshed: nothing to do");
}

#[tokio::test]
async fn an_expired_session_is_not_found() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let signed_in = account::sign_in(&db, Provider::GitHub, &fresh("gh"), "erin")
        .await
        .expect("sign in");
    let id = session::start(&db, signed_in.account_id)
        .await
        .expect("start");
    age_session(&db, &id, "-1 second").await;
    assert!(session::find(&db, &id).await.expect("lookup").is_none());
    // The next sign-in of the account sweeps its expired rows.
    session::start(&db, signed_in.account_id)
        .await
        .expect("start");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM session WHERE account_id = $1")
        .bind(signed_in.account_id)
        .fetch_one(&db)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}
