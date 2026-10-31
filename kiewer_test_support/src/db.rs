//! Postgres for the tests that need it: the pool from `DATABASE_URL`, and
//! tokens no earlier run used, since rows persist between runs on a dev
//! box.

use kiewer_lib::session::SessionId;
use sqlx::PgPool;

/// A pool on `DATABASE_URL` with the migrations run, or `None` with the
/// variable unset or empty, in which case the test prints that it skips
/// (README, Development: a box without Postgres skips, CI runs).
///
/// # Panics
///
/// When the variable is set and Postgres cannot be reached or migrated:
/// in a test, that is the failure.
pub async fn pool_from_env() -> Option<PgPool> {
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

/// `prefix-` and sixteen random hex characters: a provider user id or a
/// handle no earlier run used.
#[must_use]
pub fn fresh(prefix: &str) -> String {
    let random = SessionId::random().to_string();
    format!("{prefix}-{}", &random[..16])
}
