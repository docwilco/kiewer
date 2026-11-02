//! The HTTP side: the router, its handlers and the server loop. Sign-in,
//! the welcome page, sign-out and `/api/me` are in `web/auth.rs`; the
//! settings pages in `web/settings.rs`; the viewer page and its assets
//! in `web/viewer.rs`; the renderer beacon in `web/beacon.rs`; the about
//! page in `web/about.rs`.

mod about;
mod auth;
pub mod beacon;
mod settings;
pub mod viewer;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tokio::net::TcpListener;

use crate::blob::BlobStore;
use crate::error::{Error, Result};

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub blobs: Arc<dyn BlobStore>,
    pub auth: Arc<crate::auth::Auth>,
}

/// The routes over `state`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/about", get(about::page))
        .route("/healthz", get(healthz))
        .route("/api/me", get(auth::me))
        .route("/auth/sign-out", post(auth::sign_out))
        .route("/auth/{provider}", get(auth::begin))
        .route("/auth/{provider}/callback", get(auth::callback))
        .route("/welcome", get(auth::welcome).post(auth::choose_handle))
        .route("/settings", get(settings::account))
        .route("/settings/sessions", get(settings::sessions))
        .route("/settings/sessions/revoke", post(settings::revoke))
        .route(
            "/settings/sessions/revoke-others",
            post(settings::revoke_others),
        )
        .merge(viewer::routes())
        .merge(beacon::routes())
        .with_state(state)
}

/// Serve `state`'s routes on `listener` until `shutdown` resolves, then
/// finish the requests in flight and return. The caller picks the
/// signal: the process's SIGTERM and ctrl-c in production
/// ([`shutdown_signal`]), a channel in a test.
///
/// # Errors
///
/// When the listener fails: a closed socket, a dropped interface.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Resolves on SIGTERM (what Cloud Run sends at shutdown, forwarded by
/// tini) or ctrl-c.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for ctrl-c; stopping on SIGTERM only");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "could not listen for SIGTERM; stopping on ctrl-c only");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => tracing::info!("ctrl-c; shutting down"),
        () = terminate => tracing::info!("SIGTERM; shutting down"),
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct Index {
    build: String,
    /// The providers with an app registered: one sign-in link each. Server
    /// configuration, the same for everyone, so the page stays cacheable
    /// (ADR 0003).
    providers: Vec<crate::auth::Provider>,
}

async fn index(State(state): State<AppState>) -> Result<Html<String>> {
    let page = Index {
        build: crate::build::line(),
        providers: state.auth.providers(),
    };
    Ok(Html(page.render()?))
}

/// How long the health route waits for the database before calling it
/// down: a pool whose connections fail keeps retrying until its acquire
/// timeout, and a health check needs its answer sooner than that.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// `ok` when the database answers; 503 when it does not, so a deploy's
/// health check and a dev box's first run both say which half is up.
async fn healthz(State(state): State<AppState>) -> Response {
    let ping = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&state.db);
    match tokio::time::timeout(HEALTH_TIMEOUT, ping).await {
        Ok(Ok(_)) => (StatusCode::OK, "ok\n").into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "health: the database did not answer");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable\n").into_response()
        }
        Err(_) => {
            tracing::warn!(timeout = ?HEALTH_TIMEOUT, "health: the database did not answer in time");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable\n").into_response()
        }
    }
}

/// A handler's error is a 500 with the message in the log, never in the
/// body: the body is cacheable (ADR 0003) and the message may name paths.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
    }
}
