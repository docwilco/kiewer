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
use super::design::{design_page, design_url};
use super::open_graph::{self, OpenGraph};
use crate::account::Handle;
use crate::cache;
use crate::description::{self, Description};
use crate::design::{self, Design, DesignHead, ForkSource, Slug};
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
    /// The name, as the row has it or as the refused attempt typed it.
    pub name: String,
    /// The slug as text, not a [`Slug`]: a refused attempt may be one
    /// that does not parse, and the form shows it back as typed.
    pub slug: String,
    /// The description's Markdown.
    pub description: String,
    /// Why the last save was refused, or none; set, it opens the block.
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

/// The design page a refused save shows again, but for its settings
/// block.
struct RefusedPage<'a> {
    head: &'a DesignHead,
    description: &'a Description,
    forked_from: Option<&'a ForkSource>,
    og: OpenGraph,
}

/// The page again with the block open on `reason`, as the person's own
/// response with `status`.
fn refused(
    settled: Settled,
    status: StatusCode,
    page: RefusedPage<'_>,
    attempt: SettingsView,
    reason: String,
) -> Result<Response> {
    let settings = SettingsView {
        error: Some(reason),
        ..attempt
    };
    let page = design_page(
        page.head,
        page.description,
        settings,
        page.forked_from,
        page.og,
    )?;
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
    let forked_from = design::fork_source(&state.db, shown.design.id).await?;
    let attempt = SettingsView {
        action: url(&handle, &slug),
        name: form.name.clone(),
        slug: form.slug.trim().to_owned(),
        description: form.description.clone(),
        error: None,
    };
    // The slug that does not parse is a malformed value like any the
    // save refuses, so both go through the one match below.
    let outcome = match attempt.slug.parse::<Slug>() {
        Ok(new_slug) => {
            let settings = Settings {
                name: form.name,
                slug: new_slug,
                description: form.description,
            };
            design_settings::save(&state.db, settled.account_id, &shown.design, &settings).await
        }
        Err(error) => Err(error),
    };
    let (status, reason) = match outcome {
        Ok(slug) => {
            let keys = cache::of_design(shown.design.id, &shown.owner, shown.design.visibility);
            cache::after_write(state.purge.as_ref(), &keys).await;
            return Ok(personal(
                settled.jar,
                Redirect::to(&format!("/{handle}/{slug}")),
            ));
        }
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, design = %slug, "design settings refused");
            return Ok(personal(
                settled.jar,
                (StatusCode::FORBIDDEN, format!("{error}\n")),
            ));
        }
        Err(Error::Malformed(reason)) => (StatusCode::UNPROCESSABLE_ENTITY, reason),
        Err(Error::Refused(refusal)) => {
            tracing::info!(%refusal, "design settings refused");
            (StatusCode::CONFLICT, refusal.to_string())
        }
        Err(error) => return Err(error),
    };
    // After the outcome, so a read that fails here cannot turn a save
    // that committed into a 500: the page shown again is the design's
    // own, so its tags name that page.
    let og =
        open_graph::for_design(&state, &shown, &description, &design_url(&handle, &slug)).await?;
    let page = RefusedPage {
        head: &shown,
        description: &description,
        forked_from: forked_from.as_ref(),
        og,
    };
    refused(settled, status, page, attempt, reason)
}
