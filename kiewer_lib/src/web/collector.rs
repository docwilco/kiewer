//! `POST /api/collector`: one run of the collector (`crate::collector`),
//! which Cloud Scheduler calls each night (infra/run.tf). The caller
//! presents the token the thumbnail retry takes (`web/thumbnail.rs`), the
//! runtime service account's, so a deployment without the account closes
//! this route too and anyone else gets 401 before any work; on a dev box
//! it is open, and a POST by hand is how a run happens. 200 with what was
//! removed; a run that fails is a 500, which the scheduler shows, and the
//! next night finishes it.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use super::AppState;
use super::auth::task_caller;
use crate::cache::Policy;
use crate::collector;
use crate::error::Result;

/// The collector's route, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/collector", post(run))
}

async fn run(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    if !task_caller(&state, &headers).await? {
        return Ok((
            StatusCode::UNAUTHORIZED,
            [Policy::Personal.header()],
            "the collector takes the scheduler's token\n",
        )
            .into_response());
    }
    let collected = collector::run(&state.db, state.blobs.as_ref()).await?;
    tracing::info!(%collected, "collector run");
    Ok((
        StatusCode::OK,
        [Policy::Personal.header()],
        format!("removed {collected}\n"),
    )
        .into_response())
}
