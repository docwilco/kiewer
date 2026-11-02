//! The fork form: `POST /{handle}/{slug}/fork` makes the signed-in
//! person's own design from the one at the path (`crate::fork`), taking
//! its default branch or every branch as the form says, and sends the
//! browser to the fork's page. The form sits in the design page
//! (`templates/viewer.html`, the fork block), which renders the same for
//! everyone (ADR 0003) with the form hidden; a script shows it to anyone
//! signed in, and the `Settled` extractor is the gate. The design page
//! also carries the forked-from line, built here from
//! `design::fork_source`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Redirect, Response};
use axum::routing::post;
use axum::{Form, Router};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal};
use crate::account::Handle;
use crate::design::{Design, ForkSource, Slug};
use crate::error::{Error, Refusal, Result};
use crate::fork::{self, Take};

/// The fork route, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/{handle}/{slug}/fork", post(make))
}

/// The URL the fork form posts to.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/fork")
}

/// The fork form as the page shows it: where it posts and what the
/// branch choice offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkView {
    pub action: String,
    /// The default branch's name, which the first option names.
    pub default_branch: String,
    /// How many branches the design has: the second option offers them
    /// all, when there is more than one.
    pub branch_count: usize,
}

impl ForkView {
    /// The form for `design` at `/{handle}/{slug}` with `branch_count`
    /// branches.
    #[must_use]
    pub fn of(handle: &Handle, design: &Design, branch_count: usize) -> Self {
        Self {
            action: url(handle, &design.slug),
            default_branch: design.default_branch.clone(),
            branch_count,
        }
    }
}

/// The forked-from line as the page shows it: the source named as
/// `handle/slug`, linked while it exists, and the revision the fork
/// started at, linked while the source reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkedFromView {
    pub handle: String,
    pub slug: String,
    /// The source's page, or `None` once the source is deleted.
    pub url: Option<String>,
    /// The revision's hash, 64 hex characters, or `None` once the
    /// collector removed it.
    pub revision: Option<String>,
    /// Its short form, the first 8; empty without a revision.
    pub short: String,
    /// The revision's page under the source, when the source still
    /// reaches it.
    pub revision_url: Option<String>,
}

impl ForkedFromView {
    /// The line for a fork whose source is `source`.
    #[must_use]
    pub fn of(source: &ForkSource) -> Self {
        let design_url = format!("/{}/{}", source.handle, source.slug);
        let revision_url = source
            .revision
            .filter(|_| source.reachable)
            .map(|hash| format!("{design_url}/r/{hash}"));
        Self {
            handle: source.handle.clone(),
            slug: source.slug.clone(),
            url: source.design_id.map(|_| design_url),
            revision: source.revision.map(|hash| hash.to_string()),
            short: source.revision.map(|hash| hash.short()).unwrap_or_default(),
            revision_url,
        }
    }
}

/// The form's one field.
#[derive(Debug, Deserialize)]
pub struct ForkForm {
    take: Take,
}

/// `POST /{handle}/{slug}/fork`: the design becomes a fork of the
/// person's own and the browser goes to its page. No role is needed: a
/// design with a link is readable, and the fork is written under the
/// forker's handle. 404 when there is no such design or it has no
/// revision; 409 when another fork took the slug in the same instant;
/// a form value that is not a choice is axum's 422 before this runs.
pub(super) async fn make(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<ForkForm>,
) -> Result<Response> {
    let not_found =
        |settled: Settled| personal(settled.jar, (StatusCode::NOT_FOUND, "no such design\n"));
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found(settled));
    };
    match fork::fork(&state.db, settled.account_id, &handle, &slug, form.take).await {
        Ok(forked) => {
            let next = format!("/{}/{}", settled.handle, forked.slug);
            Ok(personal(settled.jar, Redirect::to(&next)))
        }
        Err(Error::Refused(
            refusal @ (Refusal::NoSuchDesign { .. } | Refusal::NoSuchBranch { .. }),
        )) => {
            tracing::info!(%refusal, design = %slug, "fork refused");
            Ok(not_found(settled))
        }
        Err(Error::Refused(refusal @ Refusal::SlugTaken { .. })) => {
            tracing::info!(%refusal, design = %slug, "fork refused");
            Ok(personal(
                settled.jar,
                (
                    StatusCode::CONFLICT,
                    "another design took that slug at the same moment; fork again\n",
                ),
            ))
        }
        Err(error) => Err(error),
    }
}
