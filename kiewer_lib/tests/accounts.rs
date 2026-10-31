//! Accounts, identities and sessions against Postgres: the three tables
//! and the session lifecycle through `kiewer_lib::account` and
//! `kiewer_lib::session`. Gated on `DATABASE_URL` like `tests/db.rs`.
//! Rows persist between runs on a dev box, so every test picks its own
//! provider user id and handle.

use kiewer_lib::Error;
use kiewer_lib::account::{self, Handle, Linked};
use kiewer_lib::auth::{Provider, ProviderUser};
use kiewer_lib::session::{self, SessionId};
use kiewer_test_support::db::{fresh, pool_from_env};
use sqlx::PgPool;

/// What a provider said about someone: `id` under the login `login`, no
/// email.
fn user(id: &str, login: &str) -> ProviderUser {
    ProviderUser {
        id: id.to_owned(),
        login: login.to_owned(),
        email: None,
    }
}

/// The same, with an email the provider vouched for.
fn user_with_email(id: &str, login: &str, email: &str) -> ProviderUser {
    ProviderUser {
        email: Some(email.to_owned()),
        ..user(id, login)
    }
}

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
    let first = account::sign_in(&db, Provider::GitHub, &user(&user_id, "octocat"))
        .await
        .expect("a first sign-in");
    assert!(first.is_new, "the first sign-in makes the account");
    assert_eq!(first.handle, None, "no handle until the person picks one");
    assert_eq!(first.provider_login, "octocat");
    let second = account::sign_in(&db, Provider::GitHub, &user(&user_id, "octocat-renamed"))
        .await
        .expect("a second sign-in");
    assert!(!second.is_new);
    assert_eq!(second.account_id, first.account_id);
    assert_eq!(
        second.provider_login, "octocat-renamed",
        "the login the provider shows now is the one kept"
    );
    // Two identities are never merged by login.
    let other = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "octocat"))
        .await
        .expect("another person with the same login");
    assert_ne!(other.account_id, first.account_id);
}

#[tokio::test]
async fn two_accounts_sharing_an_email_stay_two_accounts() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let email = format!("{}@example.com", fresh("shared"));
    let discord = account::sign_in(
        &db,
        Provider::Discord,
        &user_with_email(&fresh("dc"), "nelly", &email),
    )
    .await
    .expect("a first sign-in with Discord");
    let google = account::sign_in(
        &db,
        Provider::Google,
        &user_with_email(&fresh("sub"), "Nelly D", &email),
    )
    .await
    .expect("a first sign-in with Google under the same email");
    assert!(discord.is_new && google.is_new);
    assert_ne!(
        discord.account_id, google.account_id,
        "an email never finds an account: linking is explicit, from settings"
    );
    // The same provider with another id and the same email: a third account.
    let discord_again = account::sign_in(
        &db,
        Provider::Discord,
        &user_with_email(&fresh("dc"), "nelly-alt", &email),
    )
    .await
    .expect("another Discord user with the same email");
    assert_ne!(discord_again.account_id, discord.account_id);
    // The email is kept on each identity, for settings, and follows the
    // provider's answer.
    let kept: Vec<Option<String>> =
        sqlx::query_scalar("SELECT email FROM identity WHERE account_id = $1")
            .bind(discord.account_id)
            .fetch_all(&db)
            .await
            .expect("select");
    assert_eq!(kept, [Some(email.clone())]);
}

#[tokio::test]
async fn a_second_provider_is_linked_to_the_account_and_signs_in_to_it() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let alice = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "alice"))
        .await
        .expect("sign in with GitHub");
    let discord = user_with_email(&fresh("dc"), "alice_d", "alice@example.com");
    let linked = account::link(&db, alice.account_id, Provider::Discord, &discord)
        .await
        .expect("link");
    assert_eq!(linked, Linked::Now);
    // Linking the same identity again is nothing new.
    let again = account::link(&db, alice.account_id, Provider::Discord, &discord)
        .await
        .expect("link again");
    assert_eq!(again, Linked::Already);
    // A sign-in with the linked provider lands on the same account.
    let back = account::sign_in(&db, Provider::Discord, &discord)
        .await
        .expect("sign in with Discord");
    assert!(!back.is_new);
    assert_eq!(back.account_id, alice.account_id);
    // Both identities are listed for the account, in link order.
    let identities = account::identities(&db, alice.account_id)
        .await
        .expect("list");
    let summary: Vec<(Provider, &str, Option<&str>)> = identities
        .iter()
        .map(|identity| {
            (
                identity.provider,
                identity.login.as_str(),
                identity.email.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (Provider::GitHub, "alice", None),
            (Provider::Discord, "alice_d", Some("alice@example.com")),
        ]
    );
}

#[tokio::test]
async fn an_identity_on_another_account_cannot_be_linked() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let bob_google = user(&fresh("sub"), "Bob");
    let bob = account::sign_in(&db, Provider::Google, &bob_google)
        .await
        .expect("bob signs in with Google");
    let mallory = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "mallory"))
        .await
        .expect("mallory signs in with GitHub");
    let refused = account::link(&db, mallory.account_id, Provider::Google, &bob_google)
        .await
        .expect("the refusal is an outcome, not an error");
    assert_eq!(refused, Linked::ToAnotherAccount);
    let still_bob = account::sign_in(&db, Provider::Google, &bob_google)
        .await
        .expect("bob again");
    assert_eq!(still_bob.account_id, bob.account_id, "nothing moved");
    assert_eq!(
        account::identities(&db, mallory.account_id)
            .await
            .expect("list")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_handle_is_chosen_once_and_a_taken_one_is_refused() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let alice = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "alice"))
        .await
        .expect("sign in");
    let bob = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "bob"))
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
    let again = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "alice")).await;
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
    let signed_in = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "carol"))
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
    let signed_in = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "dave"))
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
    let signed_in = account::sign_in(&db, Provider::GitHub, &user(&fresh("gh"), "erin"))
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
