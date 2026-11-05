//! Postgres for the tests that need it: the pool from `DATABASE_URL`, a
//! pool on a database of a module's own beside it, and tokens no earlier
//! run used, since rows persist between runs on a dev box.

use kiewer_lib::session::SessionId;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

/// A pool on `DATABASE_URL` with the migrations run, or `None` with the
/// variable unset or empty, in which case the test prints that it skips
/// (README, Development: `scripts/checks.sh test` fails on a box without
/// Postgres unless `KIEWER_SKIP_DB_TESTS=1`, and CI runs them).
///
/// # Panics
///
/// When the variable is set and Postgres cannot be reached or migrated:
/// in a test, that is the failure.
pub async fn pool_from_env() -> Option<PgPool> {
    let url = database_url()?;
    Some(
        kiewer_lib::db::connect(&url)
            .await
            .expect("connect and migrate"),
    )
}

/// A pool on a database of the caller's own, with the migrations run: the
/// one `DATABASE_URL` names with `_<suffix>` appended (`kiewer_collector`
/// on CI), created when missing. For a module whose tests act on the
/// whole database, such as the collector's: every module's tests share
/// one binary and run at once (#100), so on the shared database another
/// module's rows would land in the middle of theirs (#107). Nothing else
/// writes to it, so a whole-database assertion stays exact. `None`, and
/// the skip, as for [`pool_from_env`].
///
/// # Panics
///
/// When the variable is set and is no Postgres URL, or Postgres cannot be
/// reached, refuses the database (the role needs CREATEDB) or cannot be
/// migrated: in a test, that is the failure.
pub async fn pool_of_its_own(suffix: &str) -> Option<PgPool> {
    let url = database_url()?;
    let shared: PgConnectOptions = url.parse().expect("DATABASE_URL is a Postgres URL");
    // With no database in the URL, sqlx takes PGDATABASE, and without that
    // Postgres connects to the user's.
    let shared_name = shared.get_database().unwrap_or(shared.get_username());
    let own = format!("{shared_name}_{suffix}");
    let mut on_shared = PgConnection::connect_with(&shared)
        .await
        .expect("connect to DATABASE_URL");
    // Postgres has no CREATE DATABASE IF NOT EXISTS: the create runs every
    // time and an existing database is the answer for every call but the
    // first. A concurrent creator surfaces as duplicate_database (42P04)
    // or, when both pass the name check, as the unique violation (23505)
    // on pg_database's name index.
    // AssertSqlSafe: the name is a quoted identifier with its quotes doubled,
    // and CREATE DATABASE takes no bind parameter.
    let create = format!("CREATE DATABASE \"{}\"", own.replace('"', "\"\""));
    match sqlx::raw_sql(sqlx::AssertSqlSafe(create))
        .execute(&mut on_shared)
        .await
    {
        Ok(_) => {}
        Err(sqlx::Error::Database(error))
            if matches!(error.code().as_deref(), Some("42P04" | "23505")) => {}
        Err(error) => panic!("create {own}: {error}"),
    }
    on_shared.close().await.expect("close");
    let pool = PgPoolOptions::new()
        .connect_with(shared.database(&own))
        .await
        .expect("connect");
    kiewer_lib::db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

/// `DATABASE_URL`, or `None` with a note that the test skips when it is
/// unset or empty.
fn database_url() -> Option<String> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty());
    if url.is_none() {
        eprintln!("DATABASE_URL is unset; skipping");
    }
    url
}

/// `prefix-` and sixteen random hex characters: a provider user id or a
/// handle no earlier run used.
#[must_use]
pub fn fresh(prefix: &str) -> String {
    let random = SessionId::random().to_string();
    format!("{prefix}-{}", &random[..16])
}
