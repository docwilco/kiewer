//! Branches and history (docs/v1-scope.md, Storage and history):
//! `/{handle}/{slug}/b/{branch}` is the viewer at the branch's head,
//! `/{handle}/{slug}/r/{hash}` the viewer at one revision the design
//! reaches, both the design page of `web/design.rs` with the branch in
//! context; `/{handle}/{slug}/b/{branch}/history` lists the branch's
//! revisions newest first with their ordinals. Ordinals are computed
//! along the branch in context and never appear in a URL (GLOSSARY,
//! Ordinal). The pages render the same for everyone.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use super::{AppState, design};
use crate::account::Handle;
use crate::design::{self as designs, At, Design, Slug};
use crate::error::Result;
use crate::revision::{Metadata, RevisionHash};

/// The branch and revision routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/b/{branch}", get(at_branch))
        .route("/{handle}/{slug}/b/{branch}/history", get(history))
        .route("/{handle}/{slug}/r/{hash}", get(at_revision))
}

/// The URL of the head of `branch` on `design`: the design's own page
/// for the default branch, `/b/{branch}` for any other.
#[must_use]
pub fn branch_url(handle: &Handle, design: &Design, branch: &str) -> String {
    if branch == design.default_branch {
        format!("/{handle}/{}", design.slug)
    } else {
        format!("/{handle}/{}/b/{branch}", design.slug)
    }
}

/// The URL of one revision of a design.
#[must_use]
pub fn revision_url(handle: &Handle, slug: &Slug, hash: RevisionHash) -> String {
    format!("/{handle}/{slug}/r/{hash}")
}

/// The URL of a branch's history page.
#[must_use]
pub fn history_url(handle: &Handle, slug: &Slug, branch: &str) -> String {
    format!("/{handle}/{slug}/b/{branch}/history")
}

/// `GET /{handle}/{slug}/b/{branch}`: the viewer at the branch's head.
async fn at_branch(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    design::show(&state, &handle, &slug, At::Branch(&branch)).await
}

/// `GET /{handle}/{slug}/r/{hash}`: the viewer at one revision, when the
/// design reaches it; 404 otherwise, and for text that is not a hash.
async fn at_revision(
    State(state): State<AppState>,
    Path((handle, slug, hash)): Path<(String, String, String)>,
) -> Result<Response> {
    let Ok(hash) = hash.parse::<RevisionHash>() else {
        return Ok((StatusCode::NOT_FOUND, "no such design\n").into_response());
    };
    design::show(&state, &handle, &slug, At::Revision(hash)).await
}

/// One row of the history page.
struct Row {
    ordinal: u32,
    short: String,
    url: String,
    author: String,
    created_at: String,
    message: Option<String>,
    /// The metadata as one line, or empty for a revision without a board.
    metadata: String,
}

/// The history page.
#[derive(Template)]
#[template(path = "history.html")]
struct HistoryPage {
    build: String,
    handle: String,
    slug: String,
    name: String,
    branch: String,
    /// The branch's head.
    head_url: String,
    /// Every branch, with its history page.
    branches: Vec<(String, String)>,
    /// Newest first.
    rows: Vec<Row>,
}

/// `GET /{handle}/{slug}/b/{branch}/history`: the branch's revisions,
/// newest first, or 404 when there is no such design or branch.
async fn history(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let not_found = || (StatusCode::NOT_FOUND, "no such branch\n").into_response();
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found());
    };
    let Some((design, owner)) = designs::find(&state.db, handle.as_str(), slug.as_str()).await?
    else {
        return Ok(not_found());
    };
    let branches = designs::branches(&state.db, design.id).await?;
    if !branches.iter().any(|known| known.name.as_str() == branch) {
        return Ok(not_found());
    }
    let entries = designs::history(&state.db, design.id, &branch, None).await?;
    let page = HistoryPage {
        build: crate::build::line(),
        handle: owner.to_string(),
        slug: design.slug.to_string(),
        name: design.name.clone(),
        head_url: branch_url(&owner, &design, &branch),
        branches: branches
            .iter()
            .map(|known| {
                (
                    known.name.to_string(),
                    history_url(&owner, &design.slug, known.name.as_str()),
                )
            })
            .collect(),
        rows: entries
            .into_iter()
            .map(|entry| Row {
                ordinal: entry.ordinal,
                short: entry.revision.hash.short(),
                url: revision_url(&owner, &design.slug, entry.revision.hash),
                author: entry.revision.author,
                created_at: entry.revision.created_at,
                message: entry.revision.message,
                metadata: entry
                    .revision
                    .metadata
                    .as_ref()
                    .map(metadata_line)
                    .unwrap_or_default(),
            })
            .collect(),
        branch,
    };
    Ok(Html(page.render()?).into_response())
}

/// The metadata as one line: its pairs joined, `Layers 26 (4 copper),
/// Nets 75, ...`.
fn metadata_line(metadata: &Metadata) -> String {
    metadata
        .details()
        .into_iter()
        .map(|(label, value)| format!("{label} {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}
