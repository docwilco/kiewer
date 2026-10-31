//! The server from configuration to shutdown: what the executable calls
//! after its build line.

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::blob::fs::FsBlobStore;
use crate::config::Config;
use crate::error::Result;
use crate::web::{AppState, serve, shutdown_signal};

/// Connect, migrate, listen on `config.port` and serve until SIGTERM or
/// ctrl-c.
///
/// # Errors
///
/// When Postgres cannot be reached or migrated, the port cannot be bound,
/// or the listener fails while serving.
pub async fn run(config: Config) -> Result<()> {
    let db = crate::db::connect(&config.database_url).await?;
    tracing::info!("database connected and migrated");
    let state = AppState {
        db,
        blobs: Arc::new(FsBlobStore::new(&config.blob_dir)),
    };
    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        blob_dir = %config.blob_dir.display(),
        "listening"
    );
    serve(listener, state, shutdown_signal()).await
}
