//! The design settings form: `POST /{handle}/{slug}/settings` saves the
//! name, the slug and the description from the settings block of the
//! design page (`templates/viewer.html`), for an admin or the owner. The
//! page itself renders the same for everyone (ADR 0003) with the block
//! hidden; a script shows it to the owner, and this handler is the gate.
//! A refusal comes back as the page with the block open and the reason,
//! as the person's own response (`private, no-store`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect, Response};
use axum::routing::post;
use axum::{Form, Router};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal};
use super::design::design_page;
use crate::account::Handle;
use crate::description::{self, Description};
use crate::design::{self, Design, DesignHead, Slug};
use crate::design_settings::{self, Settings};
use crate::error::{Error, Result};

/// The settings route, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/{handle}/{slug}/settings", post(save))
}

/// The URL the settings form posts to.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/settings")
}

/// The settings block as the page shows it: the form's values (the
/// design's own, or the refused attempt) and the reason a save was
/// refused, which also opens the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsView {
    /// Where the form posts.
    pub action: String,
    pub name: String,
    pub slug: String,
    /// The description's Markdown.
    pub description: String,
    pub error: Option<String>,
}

impl SettingsView {
    /// The block for `design` as it is.
    #[must_use]
    pub fn of(handle: &Handle, design: &Design, description: &Description) -> Self {
        Self {
            action: url(handle, &design.slug),
            name: design.name.clone(),
            slug: design.slug.to_string(),
            description: description.markdown.clone(),
            error: None,
        }
    }
}

/// The form's three fields.
#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    name: String,
    slug: String,
    description: String,
}

/// The page again with the block open on `reason`, as the person's own
/// response with `status`.
fn refused(
    settled: Settled,
    status: StatusCode,
    shown: &DesignHead,
    description: &Description,
    attempt: SettingsView,
    reason: String,
) -> Result<Response> {
    let settings = SettingsView {
        error: Some(reason),
        ..attempt
    };
    let page = design_page(shown, description, settings)?;
    Ok(personal(settled.jar, (status, Html(page))))
}

/// `POST /{handle}/{slug}/settings`: the settings become the design's
/// and the browser goes to its page, at the new slug when that moved. The
/// right to edit is the admin role or ownership, checked by the save
/// itself (`roles::require` inside its transaction); anyone below it is
/// 403. A malformed value is 422 and a slug a design of the owner's has
/// is 409, each with the page again and the reason.
pub(super) async fn save(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<SettingsForm>,
) -> Result<Response> {
    let not_found =
        |settled: Settled| personal(settled.jar, (StatusCode::NOT_FOUND, "no such design\n"));
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found(settled));
    };
    let Some(shown) = design::show(&state.db, handle.as_str(), slug.as_str()).await? else {
        return Ok(not_found(settled));
    };
    let description = description::of(&state.db, shown.design.id).await?;
    let attempt = SettingsView {
        action: url(&handle, &slug),
        name: form.name.clone(),
        slug: form.slug.trim().to_owned(),
        description: form.description.clone(),
        error: None,
    };
    let new_slug = match attempt.slug.parse::<Slug>() {
        Ok(new_slug) => new_slug,
        Err(Error::Malformed(reason)) => {
            return refused(
                settled,
                StatusCode::UNPROCESSABLE_ENTITY,
                &shown,
                &description,
                attempt,
                reason,
            );
        }
        Err(error) => return Err(error),
    };
    let settings = Settings {
        name: form.name,
        slug: new_slug,
        description: form.description,
    };
    match design_settings::save(&state.db, settled.account_id, &shown.design, &settings).await {
        Ok(slug) => Ok(personal(
            settled.jar,
            Redirect::to(&format!("/{handle}/{slug}")),
        )),
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, design = %slug, "design settings refused");
            Ok(personal(
                settled.jar,
                (StatusCode::FORBIDDEN, format!("{error}\n")),
            ))
        }
        Err(Error::Malformed(reason)) => refused(
            settled,
            StatusCode::UNPROCESSABLE_ENTITY,
            &shown,
            &description,
            attempt,
            reason,
        ),
        Err(Error::Refused(refusal)) => {
            tracing::info!(%refusal, "design settings refused");
            refused(
                settled,
                StatusCode::CONFLICT,
                &shown,
                &description,
                attempt,
                refusal.to_string(),
            )
        }
        Err(error) => Err(error),
    }
}
