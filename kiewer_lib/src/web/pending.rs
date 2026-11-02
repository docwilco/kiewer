//! The choice a pending revision waits for (`crate::pending`):
//! `/{handle}/{slug}/pending/{id}` shows its uploader what moved and
//! offers re-append and branch, each a POST of its own under that URL.
//! The upload's POST sends the browser here when its head moved. Anyone
//! else, and the uploader once it is decided or past its hour, gets a
//! 404; every response is the person's own (`private, no-store`). A
//! decision ends as a push does: the thumbnail, then the purge.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal};
use super::design::{after_landing, new_branch_name, status};
use super::history;
use crate::account::Handle;
use crate::cache;
use crate::design::Slug;
use crate::error::{Error, Refusal, Result};
use crate::pending::{self, Choice, Pending, PendingId};
use crate::revision::RevisionHash;
use crate::upload::{Author, Limits};

/// The choice's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/pending/{id}", get(page))
        .route("/{handle}/{slug}/pending/{id}/reappend", post(reappend))
        .route("/{handle}/{slug}/pending/{id}/branch", post(branch))
}

/// The URL of the choice for the pending revision `id` of a design.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug, id: PendingId) -> String {
    format!("/{handle}/{slug}/pending/{id}")
}

#[derive(Template)]
#[template(path = "pending.html")]
struct PendingPage {
    /// The signed-in person's handle.
    handle: Handle,
    /// The page's own URL, which the two forms post under.
    action: String,
    design_url: String,
    pending: Pending,
    /// The old head and the head now, short.
    base_short: String,
    head_short: String,
    /// Why the last choice was refused, or none.
    error: Option<String>,
}

fn gone(settled: Settled) -> Response {
    personal(
        settled.jar,
        (
            StatusCode::NOT_FOUND,
            "no such pending revision: it was decided, its branch was deleted, or its hour is up, so upload again\n",
        ),
    )
}

/// The page for `id`, at `status`, with `error` when a choice was
/// refused; the 404 when there is nothing to choose.
async fn render(
    state: &AppState,
    settled: Settled,
    id: PendingId,
    status: StatusCode,
    error: Option<String>,
) -> Result<Response> {
    let Some(pending) = pending::find(&state.db, id, settled.account_id).await? else {
        return Ok(gone(settled));
    };
    let page = PendingPage {
        handle: settled.handle,
        action: url(&pending.owner, &pending.design.slug, id),
        design_url: history::branch_url(&pending.owner, &pending.design, &pending.branch),
        base_short: pending.base.hash.short(),
        head_short: pending.head.hash.short(),
        pending,
        error,
    };
    Ok(personal(settled.jar, (status, Html(page.render()?))))
}

/// `GET /{handle}/{slug}/pending/{id}`: the choice, for its uploader.
async fn page(
    State(state): State<AppState>,
    Path((_, _, id)): Path<(String, String, String)>,
    settled: Settled,
) -> Result<Response> {
    let Ok(id) = id.parse::<PendingId>() else {
        return Ok(gone(settled));
    };
    render(&state, settled, id, StatusCode::OK, None).await
}

/// `choice` for `id`: to the branch's head, with the thumbnail and the
/// purge a push has; the page again with the reason of a refusal; the 403
/// of a role that is gone.
async fn decide(
    state: &AppState,
    settled: Settled,
    id: PendingId,
    choice: Result<Choice>,
) -> Result<Response> {
    let author = Author {
        account_id: settled.account_id,
        handle: settled.handle.clone(),
    };
    let decided = match choice {
        Ok(choice) => pending::decide(&state.db, &Limits::DEFAULT, &author, id, choice).await,
        Err(error) => Err(error),
    };
    let refusal = match decided {
        Ok(Some(decided)) => {
            let keys =
                cache::of_design(decided.design.id, &decided.owner, decided.design.visibility);
            after_landing(state, decided.revision, keys).await;
            let next = history::branch_url(&decided.owner, &decided.design, &decided.branch);
            return Ok(personal(settled.jar, Redirect::to(&next)));
        }
        Ok(None) => return Ok(gone(settled)),
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, "pending revision refused");
            return Ok(personal(
                settled.jar,
                (StatusCode::FORBIDDEN, format!("{error}\n")),
            ));
        }
        Err(Error::Refused(refusal)) => refusal,
        Err(error) => return Err(error),
    };
    tracing::info!(%refusal, "pending revision refused");
    render(
        state,
        settled,
        id,
        status(&refusal),
        Some(refusal.to_string()),
    )
    .await
}

/// The re-append form's field: the head the page showed.
#[derive(Debug, Deserialize)]
struct ReappendForm {
    onto: String,
}

/// `POST /{handle}/{slug}/pending/{id}/reappend`: onto the new head, as
/// long as it is still the one the page showed; else the page again,
/// showing where the branch is now.
async fn reappend(
    State(state): State<AppState>,
    Path((_, _, id)): Path<(String, String, String)>,
    settled: Settled,
    Form(form): Form<ReappendForm>,
) -> Result<Response> {
    let Ok(id) = id.parse::<PendingId>() else {
        return Ok(gone(settled));
    };
    let choice = form
        .onto
        .parse::<RevisionHash>()
        .map(|onto| Choice::Reappend { onto })
        .map_err(|_| {
            Error::Refused(Refusal::NoSuchRevision {
                hash: form.onto.clone(),
            })
        });
    decide(&state, settled, id, choice).await
}

/// The branch form's field: the new branch's name.
#[derive(Debug, Deserialize)]
struct BranchForm {
    name: String,
}

/// `POST /{handle}/{slug}/pending/{id}/branch`: a new branch at the old
/// head.
async fn branch(
    State(state): State<AppState>,
    Path((_, _, id)): Path<(String, String, String)>,
    settled: Settled,
    Form(form): Form<BranchForm>,
) -> Result<Response> {
    let Ok(id) = id.parse::<PendingId>() else {
        return Ok(gone(settled));
    };
    let choice = new_branch_name(&form.name).map(Choice::Branch);
    decide(&state, settled, id, choice).await
}
