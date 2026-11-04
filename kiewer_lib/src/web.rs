//! The HTTP side: the router, its handlers and the server loop. Sign-in,
//! the welcome page, sign-out and `/api/me` are in `web/auth.rs`; the
//! settings pages in `web/settings.rs`; the viewer page and its assets
//! in `web/viewer.rs`; the renderer beacon in `web/beacon.rs`; the about
//! page in `web/about.rs`; the design page and the upload in
//! `web/design.rs`; the blob route in `web/blob.rs`; the schematic page
//! in `web/schematic.rs`; the branch, revision and history pages in
//! `web/history.rs`; the people page in `web/people.rs`; a person's page
//! in `web/person.rs`; the design settings form in
//! `web/design_settings.rs`; the fork form in `web/fork.rs`; the
//! thumbnail route and the render retry in `web/thumbnail.rs`; the
//! Open Graph tags in `web/open_graph.rs`; a file's download in
//! `web/files.rs`; the branches page in `web/branches.rs`; the choice a
//! pending revision waits for in `web/pending.rs`; the design and account
//! delete forms in `web/delete.rs`; the collector's run in
//! `web/collector.rs`.

mod about;
mod auth;
pub mod beacon;
pub mod blob;
pub mod branches;
mod collector;
pub mod delete;
mod design;
pub mod design_settings;
pub mod files;
pub mod fork;
pub mod history;
pub mod open_graph;
pub mod pending;
mod people;
mod person;
pub mod schematic;
mod settings;
pub mod thumbnail;
pub mod viewer;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware::map_response_with_state;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tokio::net::TcpListener;

use crate::blob::BlobStore;
use crate::cache::{self, Key, KeyPrefix, Policy, Purge};
use crate::error::{Error, Result};
use crate::thumbnail::Thumbnails;

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub blobs: Arc<dyn BlobStore>,
    pub auth: Arc<crate::auth::Auth>,
    /// The thumbnail service: the renderer, its budget and the retry
    /// queue (ADR 0004).
    pub thumbnails: Arc<Thumbnails>,
    /// What a write purges at the edge (`crate::cache`): Fastly in
    /// production, nothing on a dev box, a recorder in a test.
    pub purge: Arc<dyn Purge>,
    /// What goes in front of every key a response carries
    /// (`cache::KeyPrefix`): `staging-` on kiewer-staging, nothing
    /// elsewhere. The purge client carries the same one.
    pub key_prefix: KeyPrefix,
}

impl AppState {
    /// The state over its parts. The one constructor, so a new part is
    /// added here and in `kiewer_test_support::http::state`, not in
    /// every test.
    #[must_use]
    pub fn new(
        db: PgPool,
        blobs: Arc<dyn BlobStore>,
        auth: Arc<crate::auth::Auth>,
        thumbnails: Arc<Thumbnails>,
        purge: Arc<dyn Purge>,
    ) -> Self {
        Self {
            db,
            blobs,
            auth,
            thumbnails,
            purge,
            key_prefix: KeyPrefix::default(),
        }
    }

    /// The same state with `prefix` in front of every key a response
    /// carries.
    #[must_use]
    pub fn with_key_prefix(self, prefix: KeyPrefix) -> Self {
        Self {
            key_prefix: prefix,
            ..self
        }
    }
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
        .merge(schematic::routes())
        .merge(history::routes())
        .merge(files::routes())
        .merge(people::routes())
        .merge(design_settings::routes())
        .merge(fork::routes())
        .merge(branches::routes())
        .merge(pending::routes())
        .merge(thumbnail::routes())
        .merge(delete::routes())
        .merge(collector::routes())
        // The upload's body may hold a revision's worth of files; every
        // other route keeps axum's default limit. Static routes above win
        // over the design page's two segments (axum 0.8 matches the most
        // specific route), so /settings/sessions is never a design.
        .route(
            "/upload",
            get(design::upload_form)
                .post(design::upload)
                .layer(DefaultBodyLimit::max(crate::upload::BODY_LIMIT)),
        )
        .route("/blob/{hash}", get(blob::serve))
        .route("/{handle}", get(person::page))
        .route("/{handle}/{slug}", get(design::page))
        .fallback(no_route)
        // Over every route and the fallback: the keys a handler names
        // get the environment's prefix on the way out.
        .layer(map_response_with_state(
            state.key_prefix.clone(),
            cache::prefix_surrogate_keys,
        ))
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
    /// The public designs pushed to most recently (`listing`): the same
    /// for everyone too, since no listing reads the session.
    recent: Vec<crate::listing::Listed>,
}

/// A path no route has: a 404 kept briefly, like any other.
async fn no_route() -> Response {
    cache::not_found("not found\n")
}

async fn index(State(state): State<AppState>) -> Result<Response> {
    let page = Index {
        build: crate::build::line(),
        providers: state.auth.providers(),
        recent: crate::listing::recent_public(&state.db).await?,
    };
    Ok(cache::page(&[Key::Home], Html(page.render()?)))
}

/// How long the health route waits for the database before calling it
/// down: a pool whose connections fail keeps retrying until its acquire
/// timeout, and a health check needs its answer sooner than that.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

fn unreachable_database() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [Policy::Personal.header()],
        "database unreachable\n",
    )
        .into_response()
}

/// `ok` when the database answers; 503 when it does not, so a deploy's
/// health check and a dev box's first run both say which half is up.
async fn healthz(State(state): State<AppState>) -> Response {
    let ping = sqlx::query_scalar!(r#"SELECT 1 AS "one!""#).fetch_one(&state.db);
    match tokio::time::timeout(HEALTH_TIMEOUT, ping).await {
        Ok(Ok(_)) => (StatusCode::OK, [Policy::Personal.header()], "ok\n").into_response(),
        Ok(Err(error)) => {
            tracing::warn!(%error, "health: the database did not answer");
            unreachable_database()
        }
        Err(_) => {
            tracing::warn!(timeout = ?HEALTH_TIMEOUT, "health: the database did not answer in time");
            unreachable_database()
        }
    }
}

/// A handler's error is a 500 with the message in the log, never in the
/// body: the body is cacheable (ADR 0003) and the message may name paths.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [Policy::Personal.header()],
            "internal error\n",
        )
            .into_response()
    }
}
