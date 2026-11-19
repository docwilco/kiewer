//! The server from configuration to shutdown: what the executable calls
//! after its build line.

use std::pin::pin;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::auth::Auth;
use crate::blob::BlobStore;
use crate::blob::fs::FsBlobStore;
use crate::blob::gcs::GcsBlobStore;
use crate::cache::{Fastly, NoPurge, Purge};
use crate::config::{BlobStoreConfig, Config};
use crate::error::Result;
use crate::thumbnail::{CloudTasks, NoTasks, TaskAuth, Thumbnails};
use crate::web::{AppState, Stage, router, serve_staged, shutdown_signal};

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

/// The purge client the configuration names: Fastly with the pair, else
/// none, which the log says once at startup so a deployment without its
/// token is seen. Fastly purges with the key prefix the pages' headers
/// carry ([`run`] gives the state the same one).
///
/// # Errors
///
/// When the HTTP client cannot be built.
fn purge(config: &Config) -> Result<Arc<dyn Purge>> {
    Ok(if let Some(fastly) = &config.fastly {
        let prefix = &config.surrogate_key_prefix;
        tracing::info!(service_id = %fastly.service_id, ?prefix, "purging on Fastly");
        Arc::new(Fastly::new(
            &fastly.service_id,
            &fastly.api_token,
            prefix.clone(),
        )?)
    } else {
        tracing::info!("no edge to purge: FASTLY_SERVICE_ID and FASTLY_API_TOKEN are not both set");
        Arc::new(NoPurge)
    })
}

/// Listen on `config.port`, then connect, migrate, open the blob store
/// and serve the real router, until SIGTERM or ctrl-c. Until the router
/// is up every path answers 503 with the step ([`Stage`]).
///
/// The database work runs in this future beside the listener, never in a
/// detached task: a failure there stops the listener and returns here, so
/// the process exits non-zero, the one quick "failed" Cloud Run reads.
///
/// # Errors
///
/// When the port cannot be bound, Postgres cannot be reached or migrated,
/// the GCS client cannot be built, or the listener fails while serving.
pub async fn run(config: Config) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        blobs = %config.blobs,
        "listening"
    );
    let (steps, watching) = watch::channel(Stage::Connecting);
    let mut server = pin!(serve_staged(listener, watching, shutdown_signal()));
    let mut start = pin!(start(&config, &steps));
    tokio::select! {
        result = &mut server => result,
        started = &mut start => {
            started?;
            server.await
        }
    }
}

/// The database work and the rest of the state, publishing each step on
/// `steps`, ending with the router.
async fn start(config: &Config, steps: &watch::Sender<Stage>) -> Result<()> {
    let db = crate::db::pool(&config.database_url).await?;
    steps.send_replace(Stage::Migrating);
    crate::db::migrate(&db).await?;
    tracing::info!("database connected and migrated");
    let blobs = blob_store(&config.blobs).await?;
    let state = AppState::new(
        db,
        blobs,
        Arc::new(auth(config)?),
        Arc::new(thumbnails(config)?),
        purge(config)?,
    )
    .with_key_prefix(config.surrogate_key_prefix.clone());
    steps.send_replace(Stage::Ready(router(state)));
    Ok(())
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
