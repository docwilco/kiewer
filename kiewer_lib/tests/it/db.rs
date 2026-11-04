//! Postgres: the pool connects and the migration set runs. Gated on
//! `DATABASE_URL`: without it the test returns early, and
//! `scripts/checks.sh test` fails unless `KIEWER_SKIP_DB_TESTS=1` says to
//! skip (#68). CI sets it against a service container, a dev box against
//! docker compose (README, Development).

use std::sync::Arc;

use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_test_support::db::pool_from_env;
use kiewer_test_support::{http, scratch_dir};

#[tokio::test]
async fn connects_and_runs_the_migrations() {
    let Some(pool) = pool_from_env().await else {
        return;
    };
    // Running the set twice is a no-op: every migration is recorded once.
    kiewer_lib::db::migrate(&pool)
        .await
        .expect("a second run changes nothing");
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("the migrations table exists after a run");
    assert_eq!(
        applied,
        i64::try_from(kiewer_lib::db::MIGRATOR.iter().count()).expect("a small count"),
        "every migration in migrations/ is recorded"
    );
}

#[tokio::test]
async fn health_is_ok_with_a_database() {
    let Some(db) = pool_from_env().await else {
        return;
    };
    let dir = scratch_dir!();
    let state = http::state(db, Arc::new(FsBlobStore::new(dir.path())));
    let addr = http::serve_on_free_port(state).await;
    let (status, body) = http::get(addr, "/healthz").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.trim(), "ok");
}
