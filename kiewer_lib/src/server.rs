//! The server from configuration to shutdown: what the executable calls
//! after its build line.

use std::sync::Arc;

use tokio::net::TcpListener;

use crate::auth::Auth;
use crate::blob::BlobStore;
use crate::blob::fs::FsBlobStore;
use crate::blob::gcs::GcsBlobStore;
use crate::config::{BlobStoreConfig, Config};
use crate::error::Result;
use crate::thumbnail::{CloudTasks, NoTasks, TaskAuth, Thumbnails};
use crate::web::{AppState, serve, shutdown_signal};

/// The sign-in flow from the configuration: one app per provider whose
/// variables are set. With none, the server runs and every sign-in link
/// answers 404, which the log says once at startup.
fn auth(config: &Config) -> Result<Auth> {
    let apps = config
        .oauth
        .iter()
        .map(|(provider, credentials)| provider.app(credentials.clone()))
        .collect();
    let auth = Auth::new(apps, config.public_url.clone())?;
    let providers = auth.providers();
    if providers.is_empty() {
        tracing::warn!(
            "sign-in is off: no provider app is configured (OAUTH_<PROVIDER>_CLIENT_ID and _SECRET)"
        );
    } else {
        tracing::info!(?providers, public_url = %config.public_url, "sign-in on");
    }
    Ok(auth)
}

/// The thumbnail service from the configuration (ADR 0004): retries on
/// the Cloud Tasks queue with the retry path behind the queue's token
/// when the pair is set; else no queue, which the log says once, and the
/// retry path closed on a deployment ([`TaskAuth::without_queue`]).
///
/// # Errors
///
/// When the process has no credentials to call Cloud Tasks with.
fn thumbnails(config: &Config) -> Result<Thumbnails> {
    Ok(if let Some(tasks) = &config.thumbnail_tasks {
        tracing::info!(queue = %tasks.queue, account = %tasks.account, "thumbnail retries on Cloud Tasks");
        let queue = CloudTasks::new(&tasks.queue, &config.public_url, &tasks.account)?;
        Thumbnails::new(Arc::new(queue))
            .with_auth(TaskAuth::oidc(&tasks.account, &config.public_url))
    } else {
        tracing::warn!(
            "thumbnail retries are off: no task queue is configured (THUMBNAIL_QUEUE and THUMBNAIL_TASK_ACCOUNT)"
        );
        Thumbnails::new(Arc::new(NoTasks)).with_auth(TaskAuth::without_queue(&config.public_url))
    })
}

/// Connect, migrate, open the blob store, listen on `config.port` and
/// serve until SIGTERM or ctrl-c.
///
/// # Errors
///
/// When Postgres cannot be reached or migrated, the GCS client cannot be
/// built, the port cannot be bound, or the listener fails while serving.
pub async fn run(config: Config) -> Result<()> {
    let db = crate::db::connect(&config.database_url).await?;
    tracing::info!("database connected and migrated");
    let blobs = blob_store(&config.blobs).await?;
    let state = AppState::new(
        db,
        blobs,
        Arc::new(auth(&config)?),
        Arc::new(thumbnails(&config)?),
    );
    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        blobs = %config.blobs,
        "listening"
    );
    serve(listener, state, shutdown_signal()).await
}

/// The store the configuration names. The GCS client finds its
/// credentials here, so a server without any fails at start, not on the
/// first upload.
async fn blob_store(config: &BlobStoreConfig) -> Result<Arc<dyn BlobStore>> {
    Ok(match config {
        BlobStoreConfig::Filesystem { root } => Arc::new(FsBlobStore::new(root)),
        BlobStoreConfig::Gcs {
            bucket,
            emulator: None,
        } => Arc::new(GcsBlobStore::new(bucket).await?),
        BlobStoreConfig::Gcs {
            bucket,
            emulator: Some(endpoint),
        } => Arc::new(GcsBlobStore::emulated(bucket, endpoint).await?),
    })
}
