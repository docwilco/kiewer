//! The served pages, through a real listener on a free port. The database
//! is a pool that never connects (`DATABASE_URL`-free), so these run on
//! any machine; `tests/it/db.rs` has the routes that need Postgres.

use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_lib::web::{AppState, serve};
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
async fn health_reports_an_unreachable_database() {
    let (addr, _dir) = start().await;
    let (status, body) = http::get(addr, "/healthz").await;
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
