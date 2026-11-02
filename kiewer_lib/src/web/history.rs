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
use axum::response::{Html, Response};
use axum::routing::get;
use serde::Deserialize;

use super::open_graph::{OpenGraph, metadata_line};
use super::viewer::BranchView;
use super::{AppState, design};
use crate::account::Handle;
use crate::cache::{self, Key};
use crate::design::{self as designs, At, BranchName, Design, Slug};
use crate::error::Result;
use crate::revision::RevisionHash;
use crate::thumbnail::{self, Size};

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

/// The 404 under `handle`, the first segment as the URL has it.
fn not_found(handle: &str, what: &str) -> Response {
    cache::not_found(&cache::handle_key(handle), format!("no such {what}\n"))
}

/// `GET /{handle}/{slug}/b/{branch}`: the viewer at the branch's head;
/// 404 for text that is not a branch name, before any lookup.
async fn at_branch(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
    Query(linked): Query<Linked>,
) -> Result<Response> {
    let Ok(branch) = branch.parse::<BranchName>() else {
        return Ok(not_found(&handle, "branch"));
    };
    design::show(&state, &handle, &slug, At::Branch(&branch), linked.stays()).await
}

/// The query a design page URL may carry: `b`, the branch whose page
/// linked a revision, and `design`, which keeps the design page for a
/// revision with no board (one that otherwise opens on its schematic),
/// so its settings, history and upload links stay reachable. The
/// schematic page's revision form reads the same query.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Linked {
    #[serde(default)]
    pub(super) b: Option<String>,
    #[serde(default)]
    pub(super) design: Option<String>,
}

impl Linked {
    /// Whether the URL asks for the design page itself (`?design`).
    pub(super) fn stays(&self) -> bool {
        self.design.is_some()
    }
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
        return Ok(not_found(&handle, "revision"));
    };
    let stay = linked.stays();
    let branch = linked.b.and_then(|name| name.parse::<BranchName>().ok());
    let at = At::Revision {
        hash,
        branch: branch.as_ref(),
    };
    design::show(&state, &handle, &slug, at, stay).await
}

/// One row of the history page.
struct Row {
    /// The small thumbnail's URL once it is rendered (ADR 0004); the
    /// placeholder until then.
    thumbnail: Option<String>,
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
    /// The tags in the head: the branch's history, with the head's
    /// thumbnail once rendered.
    og: OpenGraph,
}

/// `GET /{handle}/{slug}/b/{branch}/history`: the branch's revisions,
/// newest first, or 404 when there is no such design or branch.
async fn history(
    State(state): State<AppState>,
    Path((text, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let not_found = |what| not_found(&text, what);
    let (Ok(handle), Ok(slug), Ok(branch)) = (
        text.parse::<Handle>(),
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
    let hashes: Vec<RevisionHash> = entries.iter().map(|entry| entry.revision.hash).collect();
    let rendered = thumbnail::rendered(&state.db, &hashes).await?;
    let public_url = state.auth.public_url();
    let mut og = OpenGraph::new(
        public_url,
        &history_url(&owner, &design.slug, &branch),
        format!("{}: history of {branch}", design.name),
        format!("{} revisions of {} on {branch}", entries.len(), design.name),
    );
    if let Some(head) = hashes.first().filter(|hash| rendered.contains(hash)) {
        let path = thumbnail::url(owner.as_str(), design.slug.as_str(), *head, Size::Large);
        og = og.with_thumbnail(public_url, &path);
    }
    let keys = [Key::Design(design.id)];
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
                thumbnail: rendered.contains(&entry.revision.hash).then(|| {
                    thumbnail::url(
                        owner.as_str(),
                        design.slug.as_str(),
                        entry.revision.hash,
                        Size::Small,
                    )
                }),
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
        og,
    };
    Ok(cache::page(&keys, Html(page.render()?)))
}
