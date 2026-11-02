//! The two delete forms: `POST /{handle}/{slug}/delete` for a design's
//! owner, from the delete block inside the design page's settings block
//! (`templates/viewer.html`), and `POST /settings/delete` for a person's
//! own account, from `templates/settings.html`. Each asks for a name
//! typed out: the page refuses to send anything else, and `crate::delete`
//! checks again. The session cookie is `SameSite=Lax`, so a POST from
//! another site arrives without it, and the `Settled` extractor sends
//! that request home with nothing touched. The rows go at once, the
//! pages through `cache::after_write`, and every response here is the
//! person's own.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Redirect, Response};
use axum::routing::post;
use axum::{Form, Router};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal, session_cookie_removal};
use crate::account::Handle;
use crate::cache;
use crate::delete::{self, Deleted};
use crate::design::{self, Design, Slug};
use crate::error::{Error, Result};

/// The delete routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/delete", post(remove_design))
        .route("/settings/delete", post(remove_account))
}

/// The delete block as the design page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteView {
    /// Where the form posts.
    pub action: String,
    /// The name to type, as the row has it: a refused settings form's
    /// attempt is not this.
    pub name: String,
}

impl DeleteView {
    /// The block for `design` at `/{handle}/{slug}`.
    #[must_use]
    pub fn of(handle: &Handle, design: &Design) -> Self {
        Self {
            action: format!("/{handle}/{}/delete", design.slug),
            name: design.name.clone(),
        }
    }
}

/// Both forms' one field: what the person typed.
#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    confirm: String,
}

/// `POST /{handle}/{slug}/delete`: the design's rows go and the browser
/// goes to the owner's page. 403 for anyone but the owner, 404 when
/// there is no such design, 422 when what was typed is not its name.
async fn remove_design(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<ConfirmForm>,
) -> Result<Response> {
    let not_found =
        |settled: Settled| personal(settled.jar, (StatusCode::NOT_FOUND, "no such design\n"));
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found(settled));
    };
    let Some((found, owner)) = design::find(&state.db, handle.as_str(), slug.as_str()).await?
    else {
        return Ok(not_found(settled));
    };
    match delete::design(&state.db, settled.account_id, found.id, &form.confirm).await {
        Ok(Deleted::Done(keys)) => {
            tracing::info!(design = %found.id, "design deleted");
            cache::after_write(state.purge.as_ref(), &keys).await;
            Ok(personal(settled.jar, Redirect::to(&format!("/{owner}"))))
        }
        Ok(Deleted::Gone) => Ok(not_found(settled)),
        Ok(Deleted::NotConfirmed) => Ok(personal(
            settled.jar,
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "that is not the design's name, so nothing was deleted\n",
            ),
        )),
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, design = %slug, "design delete refused");
            Ok(personal(
                settled.jar,
                (StatusCode::FORBIDDEN, format!("{error}\n")),
            ))
        }
        Err(error) => Err(error),
    }
}

/// `POST /settings/delete`: the signed-in person's own account goes, its
/// handle is reserved, and the browser, its cookie gone, lands on the
/// front page. Nobody else's account is reachable from here: the account
/// is the session's. 422 when what was typed is not the handle.
async fn remove_account(
    State(state): State<AppState>,
    settled: Settled,
    Form(form): Form<ConfirmForm>,
) -> Result<Response> {
    let deleted = delete::account(
        &state.db,
        settled.account_id,
        &settled.handle,
        &form.confirm,
    )
    .await?;
    let Deleted::Done(keys) = deleted else {
        return Ok(personal(
            settled.jar,
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "that is not your handle, so nothing was deleted\n",
            ),
        ));
    };
    tracing::info!(handle = %settled.handle, "account deleted");
    cache::after_write(state.purge.as_ref(), &keys).await;
    Ok(personal(
        settled.jar.remove(session_cookie_removal()),
        Redirect::to("/"),
    ))
}
