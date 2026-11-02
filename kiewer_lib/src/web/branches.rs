//! The branches page (docs/v1-scope.md, Storage and history):
//! `/{handle}/{slug}/branches` lists a design's branches with head,
//! ordinal, age and author, and offers delete, rename, set-default and
//! a new branch according to the viewer's role (`crate::branch`, which
//! makes the same check in each write's transaction). The page renders
//! the same for everyone (ADR 0003): every form is in it hidden, and its
//! script asks `/branches/allowed`, the viewer's own answer, which to
//! show, as the design page's settings block asks `/api/me`; the age
//! comes from the head's time in that script too, since a cached page
//! cannot say how long ago. A refused change comes back as the page with
//! the viewer's forms open and the reason, as their own response. `?from=`
//! opens it for a new branch at that revision: where the design page's
//! "New branch here" goes. Every write purges what a push purges: the
//! branch dropdown, the default head and the listings all read branches,
//! and a push changes this page too.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Json, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Settled, current, personal};
use super::design::{self as design_pages, new_branch_name};
use super::history;
use crate::account::{AccountId, Handle};
use crate::branch::{self, Allowed, Listed};
use crate::cache::{self, Key};
use crate::design::{self, BranchName, Design, Slug};
use crate::error::{Error, Refusal, Result};
use crate::revision::RevisionHash;
use crate::roles;

/// The branches page's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/branches", get(page).post(create))
        .route("/{handle}/{slug}/branches/allowed", get(allowed))
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
    /// When the head was pushed; the page's script says how long ago.
    pushed_at: String,
    author: String,
    is_default: bool,
    /// Where the row's forms post under: `/{handle}/{slug}/b/{branch}`.
    base: String,
    /// Which of its forms are open: none in the page everyone gets.
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
    owner: Handle,
    slug: Slug,
    name: String,
    /// The page's own URL, which the new-branch form posts to.
    action: String,
    /// Where the script asks what this viewer may do.
    allowed_url: String,
    rows: Vec<Row>,
    /// What a new branch may start at.
    starts: Vec<Start>,
    /// Whether the new-branch form is open: not in the page everyone
    /// gets.
    new_branch: bool,
    /// Why the last change was refused, or none.
    error: Option<String>,
}

/// What `account` may do on `design`, whose branches are `listed`: for
/// each branch, and whether they may make one.
async fn allowed_for(
    state: &AppState,
    design: &Design,
    account: Option<AccountId>,
    listed: &[Listed],
) -> Result<(Vec<Allowed>, bool)> {
    let role = match account {
        Some(account) => roles::role_of(&state.db, design.id, account).await?,
        None => None,
    };
    let allowed = listed
        .iter()
        .map(|branch| {
            let mine = account.is_some() && branch.created_by == account;
            let is_default = branch.name.as_str() == design.default_branch;
            Allowed::of(role, mine, is_default)
        })
        .collect();
    Ok((allowed, role.is_some()))
}

/// The page for `design`: for everyone with no `account`; with one, the
/// page a refused change comes back as, their forms open and `error`
/// saying why. `from`, a revision the design reaches, is offered first
/// for a new branch.
async fn render(
    state: &AppState,
    (design, owner): (Design, Handle),
    account: Option<AccountId>,
    from: Option<RevisionHash>,
    error: Option<String>,
) -> Result<String> {
    let listed = branch::list(&state.db, design.id).await?;
    let (allowed, new_branch) = match account {
        Some(_) => allowed_for(state, &design, account, &listed).await?,
        None => (vec![Allowed::NOTHING; listed.len()], false),
    };
    let mut starts: Vec<Start> = from
        .into_iter()
        .map(|hash| Start {
            hash: hash.to_string(),
            label: format!("revision {}", hash.short()),
            selected: true,
        })
        .collect();
    starts.extend(listed.iter().map(|branch| Start {
        hash: branch.head.to_string(),
        label: format!("the head of {}", branch.name),
        selected: from.is_none() && branch.name.as_str() == design.default_branch,
    }));
    let rows = listed
        .into_iter()
        .zip(allowed)
        .map(|(branch, allowed)| Row {
            url: history::branch_url(&owner, &design, &branch.name),
            short: branch.head.short(),
            head_url: history::revision_url(&owner, &design, branch.head, &branch.name),
            ordinal: branch.ordinal,
            pushed_at: branch.pushed_at,
            is_default: branch.name.as_str() == design.default_branch,
            base: format!("/{owner}/{}/b/{}", design.slug, branch.name),
            allowed,
            author: branch.author,
            name: branch.name,
        })
        .collect();
    let page = BranchesPage {
        action: url(&owner, &design.slug),
        allowed_url: format!("{}/allowed", url(&owner, &design.slug)),
        owner,
        name: design.name,
        slug: design.slug,
        rows,
        starts,
        new_branch,
        error,
    };
    Ok(page.render()?)
}

/// The design at the URL's two segments, with its owner's handle, or
/// `None` for text that is no handle or slug or no such design.
async fn find(state: &AppState, handle: &str, slug: &str) -> Result<Option<(Design, Handle)>> {
    if handle.parse::<Handle>().is_err() || slug.parse::<Slug>().is_err() {
        return Ok(None);
    }
    design::find(&state.db, handle, slug).await
}

/// The 404 of a form's POST, the person's own.
fn not_found(jar: CookieJar, what: &str) -> Response {
    personal(jar, (StatusCode::NOT_FOUND, format!("no such {what}\n")))
}

/// The query the page opens with: `from`, a revision to offer first.
#[derive(Debug, Default, Deserialize)]
struct Opened {
    #[serde(default)]
    from: String,
}

/// `GET /{handle}/{slug}/branches`: the list, the same for everyone. A
/// `from` that is not a revision of the design is left out.
async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    Query(opened): Query<Opened>,
) -> Result<Response> {
    let Some((design, owner)) = find(&state, &handle, &slug).await? else {
        return Ok(cache::not_found_under(&handle, "no such design\n"));
    };
    let mut from = opened.from.parse::<RevisionHash>().ok();
    if let Some(hash) = from
        && !design::reaches(&state.db, design.id, hash).await?
    {
        from = None;
    }
    let keys = [Key::Design(design.id)];
    let page = render(&state, (design, owner), None, from, None).await?;
    Ok(cache::page(&keys, Html(page)))
}

/// What one branch's forms may open for the viewer.
#[derive(Debug, Serialize)]
struct Offer {
    name: BranchName,
    #[serde(flatten)]
    allowed: Allowed,
}

/// What the viewer may do on a design.
#[derive(Debug, Serialize)]
struct Offers {
    new_branch: bool,
    branches: Vec<Offer>,
}

/// `GET /{handle}/{slug}/branches/allowed`: which forms the page's script
/// opens for the viewer, nothing for someone signed out or without a
/// role. The one request of the page that depends on the session.
async fn allowed(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    jar: CookieJar,
) -> Result<Response> {
    let (jar, session) = current(&state, jar).await?;
    let Some((design, _)) = find(&state, &handle, &slug).await? else {
        return Ok(not_found(jar, "design"));
    };
    let account = session.map(|(_, session)| session.account_id);
    let listed = branch::list(&state.db, design.id).await?;
    let (allowed, new_branch) = allowed_for(&state, &design, account, &listed).await?;
    let branches = listed
        .into_iter()
        .zip(allowed)
        .map(|(branch, allowed)| Offer {
            name: branch.name,
            allowed,
        })
        .collect();
    let offers = Offers {
        new_branch,
        branches,
    };
    Ok(personal(jar, Json(offers)))
}

/// What came of a change, as a response: back to the page, purged; the
/// 403 of a role that does not reach; the page again with the reason of
/// a refusal, still offering `from` when the form named a revision of the
/// design; any other error is the caller's 500.
async fn finish(
    state: &AppState,
    settled: Settled,
    (design, owner): (Design, Handle),
    from: Option<RevisionHash>,
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
    let mut from = from;
    if let Some(hash) = from
        && !design::reaches(&state.db, found.0.id, hash).await?
    {
        from = None;
    }
    let reason = Some(refusal.to_string());
    let page = render(state, found, Some(settled.account_id), from, reason).await?;
    let status = design_pages::status(&refusal);
    Ok(personal(settled.jar, (status, Html(page))))
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
    let at = form.from.trim().parse::<RevisionHash>();
    let outcome = async {
        let name = new_branch_name(&form.name)?;
        let at = at.as_ref().copied().map_err(|_| {
            Error::Refused(Refusal::NoSuchRevision {
                hash: form.from.clone(),
            })
        })?;
        branch::create_at(&state.db, settled.account_id, design.id, &name, at).await
    }
    .await;
    finish(&state, settled, (design, owner), at.ok(), outcome).await
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
    finish(&state, settled, (design, owner), None, outcome).await
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
    finish(&state, settled, (design, owner), None, outcome).await
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
    finish(&state, settled, (design, owner), None, outcome).await
}
