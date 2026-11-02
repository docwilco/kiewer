//! Thumbnails over HTTP (ADR 0004): `GET /thumbnail/{revision}/{size}.png`
//! serves a rendered thumbnail, immutable, since what sits under a
//! revision hash never changes; 404 with `no-store` until it is
//! rendered, so a cache never holds the miss past the render.
//! `POST /api/thumbnails/{revision}` is the retry the Cloud Tasks queue
//! calls when the upload's render ran past the budget: it draws the
//! thumbnail with no budget and answers 200 once it exists, which is
//! what the queue takes as done. The caller presents the token the queue
//! minted (`thumbnail::TaskAuth`); 401 without one that passes.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::AppState;
use super::blob::IMMUTABLE;
use crate::error::{Error, Result};
use crate::revision::RevisionHash;
use crate::thumbnail::{self, Outcome, Size};

/// The thumbnail routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/thumbnail/{hash}/{file}", get(serve))
        .route("/api/thumbnails/{hash}", post(retry))
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CACHE_CONTROL, "no-store")],
        format!("no such {what}\n"),
    )
        .into_response()
}

/// `GET /thumbnail/{hash}/{file}`: the PNG of that size, or 404 for text
/// that is not a revision hash, a size nobody renders, or a revision
/// whose thumbnail is not rendered or is blocked (`thumbnail::SHOWABLE`).
async fn serve(
    State(state): State<AppState>,
    Path((hash, file)): Path<(String, String)>,
) -> Result<Response> {
    let (Ok(hash), Some(size)) = (hash.parse::<RevisionHash>(), Size::from_file_name(&file)) else {
        return Ok(not_found("thumbnail"));
    };
    let Some(rendered) = thumbnail::of(&state.db, hash).await? else {
        return Ok(not_found("thumbnail"));
    };
    let blob = rendered.blob(size);
    let Some(bytes) = state.blobs.get(&blob).await? else {
        return Err(Error::Failed {
            reason: format!("the store has no blob {blob} for the {size} thumbnail of {hash}"),
        });
    };
    Ok((
        [
            (CONTENT_TYPE, "image/png"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (CACHE_CONTROL, IMMUTABLE),
        ],
        bytes,
    )
        .into_response())
}

/// `POST /api/thumbnails/{hash}`: render the revision's thumbnail now.
/// 401 for a caller the task auth refuses, 404 for text that is not a
/// hash or a revision nobody has, 200 with the outcome otherwise.
async fn retry(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !state.thumbnails.allows_task(bearer).await? {
        return Ok((
            StatusCode::UNAUTHORIZED,
            "the thumbnail retry takes the queue's token\n",
        )
            .into_response());
    }
    let Ok(hash) = hash.parse::<RevisionHash>() else {
        return Ok(not_found("revision"));
    };
    let outcome = state
        .thumbnails
        .render(&state.db, state.blobs.as_ref(), hash)
        .await?;
    Ok(match outcome {
        Outcome::NoSuchRevision => not_found("revision"),
        outcome => (StatusCode::OK, format!("{outcome}\n")).into_response(),
    })
}
