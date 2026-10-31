//! Postgres: the pool connects and the migration set runs. Gated on
//! `DATABASE_URL` so a machine without Postgres skips instead of failing;
//! CI sets it against a service container, a dev box against docker
//! compose (README, Development).

use std::sync::Arc;

use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::AppState;
use kiewer_test_support::{http, scratch_dir};

fn database_url() -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    if url.is_empty() {
        return None;
    }
    Some(url)
}

#[tokio::test]
async fn connects_and_runs_the_migrations() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL is unset; skipping");
        return;
    };
    let pool = kiewer_lib::db::connect(&url)
        .await
        .expect("connect and migrate");
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
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL is unset; skipping");
        return;
    };
    let dir = scratch_dir!();
    let state = AppState {
        db: kiewer_lib::db::connect(&url).await.expect("connect"),
        blobs: Arc::new(FsBlobStore::new(dir.path())),
        auth: Arc::new(
            kiewer_lib::auth::Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth"),
        ),
    };
    let addr = http::serve_on_free_port(state).await;
    let (status, body) = http::get(addr, "/healthz").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.trim(), "ok");
}
