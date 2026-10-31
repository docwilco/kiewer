//! The server from configuration to shutdown: what the executable calls
//! after its build line.

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::auth::{Auth, github};
use crate::blob::fs::FsBlobStore;
use crate::config::Config;
use crate::error::Result;
use crate::web::{AppState, serve, shutdown_signal};

/// The sign-in flow from the configuration: one app per provider whose
/// variables are set. With none, the server runs and every sign-in link
/// answers 404, which the log says once at startup.
fn auth(config: &Config) -> Result<Auth> {
    let mut apps = Vec::new();
    if let Some(app) = &config.github {
        apps.push(github::app(
            app.client_id.clone(),
            app.client_secret.clone(),
        ));
    }
    let auth = Auth::new(apps, config.public_url.clone())?;
    let providers = auth.providers();
    if providers.is_empty() {
        tracing::warn!("sign-in is off: no provider app is configured (GITHUB_CLIENT_ID)");
    } else {
        tracing::info!(?providers, public_url = %config.public_url, "sign-in on");
    }
    Ok(auth)
}

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
        auth: Arc::new(auth(&config)?),
    };
    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        blob_dir = %config.blob_dir.display(),
        "listening"
    );
    serve(listener, state, shutdown_signal()).await
}
