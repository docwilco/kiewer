//! Thumbnails over HTTP (ADR 0004):
//! `GET /{handle}/{slug}/r/{revision}/thumbnail/{size}.png` serves a
//! rendered thumbnail of a revision the design reaches, immutable, since
//! the pixels under a revision hash never change; 404 with `no-store`
//! until it is rendered, or once a file of the revision is blocked, so
//! no cache holds the miss past the render. `POST
//! /api/thumbnails/{revision}` is the retry the Cloud Tasks queue calls
//! when the upload's render ran past the budget or failed: it draws the
//! thumbnail with no budget and answers 200 once there is nothing left
//! to do, which is what the queue takes as done. The caller presents the
//! token the queue minted (`thumbnail::TaskAuth`); 401 without one that
//! passes.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use super::AppState;
use super::auth::task_caller;
use crate::account::Handle;
use crate::cache::{self, Key, Policy};
use crate::design::{self, Slug};
use crate::error::{Error, Result};
use crate::revision::RevisionHash;
use crate::thumbnail::{self, Outcome, Size};

/// The thumbnail routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/r/{hash}/thumbnail/{file}", get(serve))
        .route("/api/thumbnails/{hash}", post(retry))
}

fn not_found(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [Policy::Personal.header()],
        format!("no such {what}\n"),
    )
        .into_response()
}

/// `GET /{handle}/{slug}/r/{hash}/thumbnail/{file}`: the PNG of that
/// size, or 404 for a segment that is none of those, a size nobody
/// renders, a revision the design does not reach, or one with no
/// thumbnail to show (`thumbnail::SHOWABLE`).
async fn serve(
    State(state): State<AppState>,
    Path((handle, slug, hash, file)): Path<(String, String, String, String)>,
) -> Result<Response> {
    let (Ok(handle), Ok(slug), Ok(hash), Some(size)) = (
        handle.parse::<Handle>(),
        slug.parse::<Slug>(),
        hash.parse::<RevisionHash>(),
        Size::from_file_name(&file),
    ) else {
        return Ok(not_found("thumbnail"));
    };
    let Some((found, _)) = design::find(&state.db, handle.as_str(), slug.as_str()).await? else {
        return Ok(not_found("thumbnail"));
    };
    if !design::reaches(&state.db, found.id, hash).await? {
        return Ok(not_found("thumbnail"));
    }
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
            Policy::Immutable.header(),
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
    if !task_caller(&state, &headers).await? {
        return Ok((
            StatusCode::UNAUTHORIZED,
            [Policy::Personal.header()],
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
    if outcome == Outcome::Rendered {
        purge_holders(&state, hash).await;
    }
    Ok(match outcome {
        Outcome::NoSuchRevision => not_found("revision"),
        outcome => (
            StatusCode::OK,
            [Policy::Personal.header()],
            format!("{outcome}\n"),
        )
            .into_response(),
    })
}

/// Purge what a thumbnail that arrived after the push changes: listings
/// and Open Graph tags hold a placeholder until it exists, on every
/// design that reaches the revision, forks included. A read that fails
/// is a warning, as a failed purge is: the thumbnail stands, and the
/// pages catch up within the hour (`crate::cache::Policy::Page`).
async fn purge_holders(state: &AppState, hash: RevisionHash) {
    let holders = match design::holding(&state.db, hash).await {
        Ok(holders) => holders,
        Err(error) => {
            tracing::warn!(%error, %hash, "the purge after a late thumbnail did not start");
            return;
        }
    };
    let mut keys = vec![Key::Home];
    for (design, owner) in holders {
        keys.push(Key::Design(design));
        keys.push(Key::Handle(owner));
    }
    keys.sort_unstable();
    keys.dedup();
    cache::after_write(state.purge.as_ref(), &keys).await;
}
