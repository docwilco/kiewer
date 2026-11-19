//! The served pages, through a real listener on a free port. The database
//! is a pool that never connects (`DATABASE_URL`-free), so these run on
//! any machine; `tests/it/db.rs` has the routes that need Postgres.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::{AppState, Stage, router, serve, serve_staged};
use kiewer_test_support::{TempDir, http, scratch_dir};
use tokio::net::TcpListener;

/// A state whose pool points at a closed port, so a database touch fails
/// instead of hanging, with the blob store under cargo's scratch directory.
fn state() -> (AppState, TempDir) {
    let dir = scratch_dir!();
    let state = http::state(
        kiewer_lib::db::lazy_pool("postgres://nobody@127.0.0.1:1/none").expect("a lazy pool"),
        Arc::new(FsBlobStore::new(dir.path())),
    );
    (state, dir)
}

async fn start() -> (SocketAddr, TempDir) {
    let (state, dir) = state();
    (http::serve_on_free_port(state).await, dir)
}

#[tokio::test]
async fn an_unknown_path_is_not_found() {
    let (addr, _dir) = start().await;
    // A segment no handle can be (a dot): the person route answers before
    // any lookup, so the closed-port pool is never touched. A segment that
    // is a handle is looked up, and `tests/it/roles.rs` covers that.
    let (status, _) = http::get(addr, "/not-a-page.txt").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn the_old_health_path_is_not_found() {
    let (addr, _dir) = start().await;
    let (status, _) = http::get(addr, "/healthz").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn every_path_answers_503_with_the_step_until_the_server_is_ready() {
    let (state, _dir) = state();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (steps, watching) = tokio::sync::watch::channel(Stage::Connecting);
    tokio::spawn(serve_staged(listener, watching, std::future::pending()));
    for path in ["/health", "/about"] {
        let (status, body) = http::get(addr, path).await;
        assert_eq!((status, body.as_str()), (503, "connecting\n"), "{path}");
    }
    steps.send_replace(Stage::Migrating);
    let (status, body) = http::get(addr, "/health").await;
    assert_eq!((status, body.as_str()), (503, "migrating\n"));
    steps.send_replace(Stage::Ready(router(state)));
    let (status, body) = http::get(addr, "/about").await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn a_start_on_an_unreachable_database_returns_its_error() {
    let config = kiewer_lib::config::Config::from_vars(|name| {
        match name {
            "PORT" => Some("0"),
            // Refused as a connection string, so the error is immediate;
            // a closed port would be retried until the pool's timeout.
            "DATABASE_URL" => Some("not a connection string"),
            _ => None,
        }
        .map(str::to_owned)
    })
    .expect("a configuration");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        kiewer_lib::server::run(config),
    )
    .await
    .expect("run returns instead of serving on");
    result.expect_err("the failed start is run's error");
}

#[tokio::test]
async fn health_reports_an_unreachable_database() {
    let (addr, _dir) = start().await;
    let (status, body) = http::get(addr, "/health").await;
    assert_eq!(status, 503, "{body}");
}

#[tokio::test]
async fn the_server_stops_when_told() {
    let (state, _dir) = state();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state, async {
        let _ = rx.await;
    }));
    tx.send(()).expect("the server is waiting on the signal");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("the server returns once told to stop")
        .expect("the server task did not panic")
        .expect("the server stopped cleanly");
}
