//! The branches page (docs/v1-scope.md, Storage and history):
//! `/{handle}/{slug}/branches` lists a design's branches with head,
//! ordinal, age and author, and offers delete, rename, set-default and
//! a new branch according to the viewer's role (`crate::branch`, which
//! makes the same check in each write's transaction). Anyone may read it;
//! what it offers depends on the session, so every response here carries
//! `private, no-store` (ADR 0003), as the people page does. `?from=`
//! opens it for a new branch at that revision: where the design page's
//! "New branch here" goes. Every write purges what a push purges: the
//! branch dropdown, the default head and the listings all read branches.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, current, personal};
use super::design::{self as design_pages, new_branch_name};
use super::history;
use crate::account::{AccountId, Handle};
use crate::branch::{self, Allowed};
use crate::cache;
use crate::design::{self, BranchName, Design, Slug};
use crate::error::{Error, Refusal, Result};
use crate::revision::RevisionHash;
use crate::roles;

/// The branches page's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/branches", get(page).post(create))
        .route("/{handle}/{slug}/b/{branch}/rename", post(rename))
        .route("/{handle}/{slug}/b/{branch}/delete", post(delete))
        .route("/{handle}/{slug}/b/{branch}/default", post(set_default))
}

/// The URL of a design's branches page.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/branches")
}

/// The branches page opened for a new branch at `revision`.
#[must_use]
pub fn new_branch_url(handle: &Handle, slug: &Slug, revision: RevisionHash) -> String {
    format!("{}?from={revision}", url(handle, slug))
}

/// One branch as the page lists it.
struct Row {
    name: BranchName,
    url: String,
    short: String,
    head_url: String,
    ordinal: u32,
    age: String,
    author: String,
    is_default: bool,
    /// Where the row's forms post under: `/{handle}/{slug}/b/{branch}`.
    base: String,
    allowed: Allowed,
}

/// One revision a new branch may start at.
struct Start {
    hash: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "branches.html")]
struct BranchesPage {
    /// The signed-in person's handle, or none.
    viewer: Option<Handle>,
    owner: Handle,
    slug: Slug,
    name: String,
    /// The page's own URL, which the new-branch form posts to.
    action: String,
    rows: Vec<Row>,
    /// What a new branch may start at, or empty for someone who may not
    /// make one.
    starts: Vec<Start>,
    /// Why the last change was refused, or none.
    error: Option<String>,
}

/// Who is looking: their account and handle, or nobody.
type Viewer = Option<(AccountId, Handle)>;

/// The page's response for `design`, at `status`, with `error` when a
/// change was refused and `from`, a revision the design reaches, offered
/// first for a new branch.
async fn render(
    state: &AppState,
    jar: CookieJar,
    (design, owner): (Design, Handle),
    viewer: Viewer,
    from: Option<RevisionHash>,
    (status, error): (StatusCode, Option<String>),
) -> Result<Response> {
    let role = match &viewer {
        Some((account, _)) => roles::role_of(&state.db, design.id, *account).await?,
        None => None,
    };
    let account = viewer.as_ref().map(|(account, _)| *account);
    let listed = branch::list(&state.db, design.id).await?;
    let mut starts = Vec::new();
    if role.is_some() {
        if let Some(hash) = from {
            starts.push(Start {
                hash: hash.to_string(),
                label: format!("revision {}", hash.short()),
                selected: true,
            });
        }
        starts.extend(listed.iter().map(|branch| Start {
            hash: branch.head.to_string(),
            label: format!("the head of {}", branch.name),
            selected: from.is_none() && branch.name.as_str() == design.default_branch,
        }));
    }
    let rows = listed
        .into_iter()
        .map(|branch| {
            let is_default = branch.name.as_str() == design.default_branch;
            let mine = account.is_some() && branch.created_by == account;
            Row {
                url: history::branch_url(&owner, &design, &branch.name),
                short: branch.head.short(),
                head_url: history::revision_url(&owner, &design, branch.head, &branch.name),
                ordinal: branch.ordinal,
                age: branch.age(),
                is_default,
                base: format!("/{owner}/{}/b/{}", design.slug, branch.name),
                allowed: Allowed::of(role, mine, is_default),
                author: branch.author,
                name: branch.name,
            }
        })
        .collect();
    let page = BranchesPage {
        viewer: viewer.map(|(_, handle)| handle),
        action: url(&owner, &design.slug),
        owner,
        name: design.name,
        slug: design.slug,
        rows,
        starts,
        error,
    };
    Ok(personal(jar, (status, Html(page.render()?))))
}

/// The design at the URL's two segments, with its owner's handle, or
/// `None` for text that is no handle or slug or no such design.
async fn find(state: &AppState, handle: &str, slug: &str) -> Result<Option<(Design, Handle)>> {
    if handle.parse::<Handle>().is_err() || slug.parse::<Slug>().is_err() {
        return Ok(None);
    }
    design::find(&state.db, handle, slug).await
}

fn not_found(jar: CookieJar, what: &str) -> Response {
    personal(jar, (StatusCode::NOT_FOUND, format!("no such {what}\n")))
}

/// The query the page opens with: `from`, a revision to offer first.
#[derive(Debug, Default, Deserialize)]
struct Opened {
    #[serde(default)]
    from: String,
}

/// `GET /{handle}/{slug}/branches`: the list, for anyone; the forms, for
/// a person with a role. A `from` that is not a revision of the design
/// is left out.
async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    Query(opened): Query<Opened>,
    jar: CookieJar,
) -> Result<Response> {
    let (jar, session) = current(&state, jar).await?;
    let Some((design, owner)) = find(&state, &handle, &slug).await? else {
        return Ok(not_found(jar, "design"));
    };
    let viewer = session.and_then(|(_, session)| Some((session.account_id, session.handle?)));
    let mut from = opened.from.parse::<RevisionHash>().ok();
    if let Some(hash) = from
        && !design::reaches(&state.db, design.id, hash).await?
    {
        from = None;
    }
    let shown = (StatusCode::OK, None);
    render(&state, jar, (design, owner), viewer, from, shown).await
}

/// What came of a change, as a response: back to the page, purged; the
/// 403 of a role that does not reach; the page again with the reason of
/// a refusal; any other error is the caller's 500.
async fn finish(
    state: &AppState,
    settled: Settled,
    (design, owner): (Design, Handle),
    outcome: Result<()>,
) -> Result<Response> {
    let refusal = match outcome {
        Ok(()) => {
            let keys = cache::of_design(design.id, &owner, design.visibility);
            cache::after_write(state.purge.as_ref(), &keys).await;
            return Ok(personal(
                settled.jar,
                Redirect::to(&url(&owner, &design.slug)),
            ));
        }
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, design = %design.slug, "branch change refused");
            return Ok(personal(
                settled.jar,
                (StatusCode::FORBIDDEN, format!("{error}\n")),
            ));
        }
        Err(Error::Refused(refusal)) => refusal,
        Err(error) => return Err(error),
    };
    tracing::info!(%refusal, design = %design.slug, "branch change refused");
    // Read afresh: a refused change leaves the design as it is, and the
    // default branch may have moved since the handler read the row.
    let found = find(state, owner.as_str(), design.slug.as_str())
        .await?
        .unwrap_or((design, owner));
    let viewer = Some((settled.account_id, settled.handle));
    let shown = (design_pages::status(&refusal), Some(refusal.to_string()));
    render(state, settled.jar, found, viewer, None, shown).await
}

/// A branch form's one field: the name to give.
#[derive(Debug, Deserialize)]
struct NameForm {
    name: String,
}

/// The new-branch form: the name, and the revision to start at.
#[derive(Debug, Deserialize)]
struct NewBranchForm {
    name: String,
    from: String,
}

/// `POST /{handle}/{slug}/branches`: a new branch pointing at a revision
/// the design reaches, with no new revision.
async fn create(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<NewBranchForm>,
) -> Result<Response> {
    let Some((design, owner)) = find(&state, &handle, &slug).await? else {
        return Ok(not_found(settled.jar, "design"));
    };
    let outcome = async {
        let name = new_branch_name(&form.name)?;
        let at = form.from.trim().parse::<RevisionHash>().map_err(|_| {
            Error::Refused(Refusal::NoSuchRevision {
                hash: form.from.clone(),
            })
        })?;
        branch::create_at(&state.db, settled.account_id, design.id, &name, at).await
    }
    .await;
    finish(&state, settled, (design, owner), outcome).await
}

/// The design and the branch of a URL under `/b/{branch}`, or `None`.
async fn on_branch(
    state: &AppState,
    handle: &str,
    slug: &str,
    branch: &str,
) -> Result<Option<(Design, Handle, BranchName)>> {
    let Ok(branch) = branch.parse::<BranchName>() else {
        return Ok(None);
    };
    Ok(find(state, handle, slug)
        .await?
        .map(|(design, owner)| (design, owner, branch)))
}

/// `POST /{handle}/{slug}/b/{branch}/rename`: for an admin, the owner,
/// or the collaborator who made the branch; the default branch for an
/// admin or the owner alone.
async fn rename(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
    settled: Settled,
    Form(form): Form<NameForm>,
) -> Result<Response> {
    let Some((design, owner, from)) = on_branch(&state, &handle, &slug, &branch).await? else {
        return Ok(not_found(settled.jar, "branch"));
    };
    let outcome = async {
        let to = new_branch_name(&form.name)?;
        branch::rename(&state.db, settled.account_id, design.id, &from, &to).await
    }
    .await;
    finish(&state, settled, (design, owner), outcome).await
}

/// `POST /{handle}/{slug}/b/{branch}/delete`: for the same people; the
/// default branch is refused for everyone.
async fn delete(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
    settled: Settled,
) -> Result<Response> {
    let Some((design, owner, name)) = on_branch(&state, &handle, &slug, &branch).await? else {
        return Ok(not_found(settled.jar, "branch"));
    };
    let outcome = branch::delete(&state.db, settled.account_id, design.id, &name).await;
    finish(&state, settled, (design, owner), outcome).await
}

/// `POST /{handle}/{slug}/b/{branch}/default`: for an admin or the owner.
async fn set_default(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
    settled: Settled,
) -> Result<Response> {
    let Some((design, owner, name)) = on_branch(&state, &handle, &slug, &branch).await? else {
        return Ok(not_found(settled.jar, "branch"));
    };
    let outcome = branch::set_default(&state.db, settled.account_id, design.id, &name).await;
    finish(&state, settled, (design, owner), outcome).await
}
