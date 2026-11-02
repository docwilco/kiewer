//! Branches and history (docs/v1-scope.md, Storage and history):
//! `/{handle}/{slug}/b/{branch}` is the viewer at the branch's head,
//! `/{handle}/{slug}/r/{hash}` the viewer at one revision the design
//! reaches, both the design page of `web/design.rs` with the branch in
//! context; `/{handle}/{slug}/b/{branch}/history` lists the branch's
//! revisions newest first with their ordinals. Ordinals are computed
//! along the branch in context and never appear in a URL (GLOSSARY,
//! Ordinal). A revision URL may carry `?b={branch}`, the branch whose
//! page linked it, so the context follows a click off a branch other
//! than the default; the path form stays the settled one. The pages
//! render the same for everyone.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use super::viewer::BranchView;
use super::{AppState, design};
use crate::account::Handle;
use crate::design::{self as designs, At, BranchName, Design, Slug};
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
pub fn branch_url(handle: &Handle, design: &Design, branch: &BranchName) -> String {
    if branch.as_str() == design.default_branch {
        format!("/{handle}/{}", design.slug)
    } else {
        format!("/{handle}/{}/b/{branch}", design.slug)
    }
}

/// The URL of one revision of `design`, linked from `branch`'s page: a
/// branch other than the default goes along as the `b` query, so the
/// page opens in that branch's context.
#[must_use]
pub fn revision_url(
    handle: &Handle,
    design: &Design,
    hash: RevisionHash,
    branch: &BranchName,
) -> String {
    if branch.as_str() == design.default_branch {
        format!("/{handle}/{}/r/{hash}", design.slug)
    } else {
        format!("/{handle}/{}/r/{hash}?b={branch}", design.slug)
    }
}

/// The URL of a branch's history page.
#[must_use]
pub fn history_url(handle: &Handle, slug: &Slug, branch: &BranchName) -> String {
    format!("/{handle}/{slug}/b/{branch}/history")
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("no such {what}\n")).into_response()
}

/// `GET /{handle}/{slug}/b/{branch}`: the viewer at the branch's head;
/// 404 for text that is not a branch name, before any lookup.
async fn at_branch(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let Ok(branch) = branch.parse::<BranchName>() else {
        return Ok(not_found("branch"));
    };
    design::show(&state, &handle, &slug, At::Branch(&branch)).await
}

/// The query a revision URL may carry: the branch whose page linked it.
#[derive(Debug, Default, Deserialize)]
struct Linked {
    #[serde(default)]
    b: Option<String>,
}

/// `GET /{handle}/{slug}/r/{hash}`: the viewer at one revision, when the
/// design reaches it; 404 otherwise, and for text that is not a hash. A
/// `b` query that is not a branch name is ignored, as one naming a
/// branch that does not reach the revision is.
async fn at_revision(
    State(state): State<AppState>,
    Path((handle, slug, hash)): Path<(String, String, String)>,
    Query(linked): Query<Linked>,
) -> Result<Response> {
    let Ok(hash) = hash.parse::<RevisionHash>() else {
        return Ok(not_found("revision"));
    };
    let branch = linked.b.and_then(|name| name.parse::<BranchName>().ok());
    let at = At::Revision {
        hash,
        branch: branch.as_ref(),
    };
    design::show(&state, &handle, &slug, at).await
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
    /// Every branch, each linking its history page.
    branches: Vec<BranchView>,
    /// Newest first.
    rows: Vec<Row>,
}

/// `GET /{handle}/{slug}/b/{branch}/history`: the branch's revisions,
/// newest first, or 404 when there is no such design or branch.
async fn history(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let (Ok(handle), Ok(slug), Ok(branch)) = (
        handle.parse::<Handle>(),
        slug.parse::<Slug>(),
        branch.parse::<BranchName>(),
    ) else {
        return Ok(not_found("branch"));
    };
    let Some((design, owner)) = designs::find(&state.db, handle.as_str(), slug.as_str()).await?
    else {
        return Ok(not_found("branch"));
    };
    let branches = designs::branches(&state.db, design.id).await?;
    if !branches.iter().any(|known| known.name == branch) {
        return Ok(not_found("branch"));
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
            .map(|known| BranchView {
                name: known.name.to_string(),
                url: history_url(&owner, &design.slug, &known.name),
            })
            .collect(),
        rows: entries
            .into_iter()
            .map(|entry| Row {
                ordinal: entry.ordinal,
                short: entry.revision.hash.short(),
                url: revision_url(&owner, &design, entry.revision.hash, &branch),
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
        branch: branch.to_string(),
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
