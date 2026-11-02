//! The design page and the upload: `/{handle}/{slug}` shows the head of
//! the design's default branch in the viewer with the revision's details
//! (`web/viewer.rs`'s page); `/upload` is the form and its POST for a
//! signed-in person with a handle, streamed part by part into
//! `upload::Incoming` and committed by `upload::push`. The upload pages
//! are the person's own, so they carry `private, no-store`; the design
//! page renders the same for everyone.

use askama::Template;
use axum::extract::multipart::{Field, Multipart, MultipartError};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::LOCATION;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::auth::{Settled, personal};
use super::design_settings::SettingsView;
use super::viewer::{BranchView, DesignView, FileView, Page, RecentView};
use super::{AppState, blob, history, schematic};
use crate::account::Handle;
use crate::description::{self, Description};
use crate::design::{self, At, BranchName, DesignHead, Slug};
use crate::error::{Error, Refusal, Result};
use crate::revision::RevisionHash;
use crate::roles::{self, Membership};
use crate::slug;
use crate::upload::{self, Author, BranchTarget, Incoming, Limits, Target};

/// The URL of a design's page.
fn design_url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}")
}

/// The upload form opened from a design page: for `branch` (`None` for a
/// new branch) from `revision`, the one the page showed, which a new
/// branch starts from. The handle, the slug and the branch name are their
/// own characters; the hash is hex.
fn upload_url(
    handle: &Handle,
    slug: &Slug,
    branch: Option<&BranchName>,
    revision: RevisionHash,
) -> String {
    format!(
        "/upload?design={handle}/{slug}&branch={}&from={revision}",
        branch.map(BranchName::as_str).unwrap_or_default()
    )
}

/// The URL of a design's people page.
pub(super) fn people_url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/people")
}

/// The design page for `shown`: the viewer at the revision shown, in the
/// context of its branch, with the revision's details, the design's
/// `description`, and `settings` as its settings block. Rendered here for
/// the page, the branch and revision routes (`web/history.rs`) and the
/// settings form's refusal, which shows the page again with the reason.
pub(super) fn design_page(
    shown: &DesignHead,
    description: &Description,
    settings: SettingsView,
) -> Result<String> {
    let board_url = shown.board().map(|entry| blob::url(&entry.blob));
    let schematic_url = shown
        .schematic()
        .map(|_| schematic::url(&shown.owner, &shown.design.slug));
    let at_branch = |name: &BranchName| history::branch_url(&shown.owner, &shown.design, name);
    let recent_view = |ordinal: u32, revision: &design::Revision| RecentView {
        ordinal,
        short: revision.hash.short(),
        url: history::revision_url(&shown.owner, &shown.design, revision.hash, &shown.branch),
        created_at: revision.created_at.clone(),
        current: revision.hash == shown.revision.hash,
    };
    let mut recent: Vec<RecentView> = shown
        .recent
        .iter()
        .map(|entry| recent_view(entry.ordinal, &entry.revision))
        .collect();
    // A revision older than the last ten is still the one shown: it joins
    // the list at the end, so the dropdown has it selected.
    if !recent.iter().any(|entry| entry.current) {
        recent.push(recent_view(shown.ordinal, &shown.revision));
    }
    let view = DesignView {
        handle: shown.owner.to_string(),
        slug: shown.design.slug.to_string(),
        branches: shown
            .branches
            .iter()
            .map(|branch| BranchView {
                name: branch.name.to_string(),
                url: at_branch(&branch.name),
            })
            .collect(),
        recent,
        ordinal: shown.ordinal,
        behind: shown.behind,
        head_url: at_branch(&shown.branch),
        history_url: history::history_url(&shown.owner, &shown.design.slug, &shown.branch),
        upload_url: upload_url(
            &shown.owner,
            &shown.design.slug,
            Some(&shown.branch),
            shown.revision.hash,
        ),
        new_branch_url: upload_url(&shown.owner, &shown.design.slug, None, shown.revision.hash),
        branch: shown.branch.to_string(),
        revision: shown.revision.hash.to_string(),
        short: shown.revision.hash.short(),
        author: shown.revision.author.clone(),
        created_at: shown.revision.created_at.clone(),
        message: shown.revision.message.clone(),
        details: shown
            .revision
            .metadata
            .as_ref()
            .map(crate::revision::Metadata::details)
            .unwrap_or_default(),
        files: shown
            .files
            .iter()
            .map(|entry| FileView {
                path: entry.path.to_string(),
                name: entry.path.file_name().to_owned(),
                url: blob::url(&entry.blob),
                size: entry.size,
            })
            .collect(),
        schematic_url,
        visibility: shown.design.visibility,
        people_url: people_url(&shown.owner, &shown.design.slug),
        description_html: description.html.clone(),
        settings,
    };
    let page = Page::new(shown.design.name.clone(), board_url, Some(view));
    Ok(page.render()?)
}

/// `GET /{handle}/{slug}`: the viewer at the head of the default branch
/// with the revision's details, or 404 when there is no such design or
/// it has no revision yet. A slug the design was at before answers 301
/// to where it is now, until another design of the owner's takes the
/// slug (`crate::slug`). Two segments that are not a handle and a slug
/// (a reserved word such as `viewer`, a file name) are 404 before any
/// lookup: the route is the last one, so every other path lands here.
/// An unlisted design opens here like a public one: visibility is about
/// listings, authorisation about writes (docs/v1-scope.md).
pub(super) async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
) -> Result<Response> {
    show(&state, &handle, &slug, At::Default).await
}

/// The design `/{handle}/{slug}` shown at `at`, as [`design_page`]
/// renders it, or 404 when there is no such design, branch or reachable
/// revision; the branch and revision routes (`web/history.rs`) land here
/// too. The slug redirect is the design page's alone: a branch or
/// revision URL under an old slug is 404, since the links a page carries
/// are under the slug it was rendered at.
pub(super) async fn show(
    state: &AppState,
    handle: &str,
    slug: &str,
    at: At<'_>,
) -> Result<Response> {
    let not_found = || (StatusCode::NOT_FOUND, "no such design\n").into_response();
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found());
    };
    let Some(shown) = design::show_at(&state.db, handle.as_str(), slug.as_str(), at).await? else {
        if at != At::Default {
            return Ok(not_found());
        }
        let moved = slug::redirect(&state.db, handle.as_str(), slug.as_str()).await?;
        return Ok(match moved {
            Some(current) => (
                StatusCode::MOVED_PERMANENTLY,
                [(LOCATION, design_url(&handle, &current))],
            )
                .into_response(),
            None => not_found(),
        });
    };
    // A revision with no board and a schematic opens on its first sheet:
    // the schematic page at the same branch or revision.
    if shown.opens_on_schematic() {
        let sheet = schematic::url_at(
            &handle,
            &shown.design.slug,
            &shown.design.default_branch,
            at,
        );
        return Ok(Redirect::to(&sheet).into_response());
    }
    let description = description::of(&state.db, shown.design.id).await?;
    let settings = SettingsView::of(&handle, &shown.design, &description);
    Ok(Html(design_page(&shown, &description, settings)?).into_response())
}

/// Where the upload form was opened from (`/upload?design=&branch=&from=`,
/// the design page's links): the design to preselect, the branch of that
/// page (empty for a new branch; absent for the design's default), and
/// the revision it showed, which a new branch starts from.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Opened {
    #[serde(default)]
    design: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    from: String,
}

#[derive(Template)]
#[template(path = "upload.html")]
struct UploadPage {
    handle: Handle,
    /// The designs the person may push to, newest first: the targets
    /// besides a new one.
    designs: Vec<Membership>,
    /// The path (`handle/slug`) to preselect, or empty for a new design.
    design: String,
    /// The branches the selector offers: the preselected design's, or the
    /// default branch's name alone when no design is in context.
    branches: Vec<String>,
    /// The branch to preselect, or empty for a new branch.
    branch: String,
    /// The revision a new branch starts from, or empty.
    from: String,
    /// Why the last attempt was refused, or none.
    error: Option<String>,
    file_mb: u64,
    revision_mb: u64,
}

async fn upload_page(
    state: &AppState,
    settled: Settled,
    opened: &Opened,
    status: StatusCode,
    error: Option<String>,
) -> Result<Response> {
    let designs = roles::designs_of(&state.db, settled.account_id).await?;
    let selected = designs.iter().find(|member| member.path() == opened.design);
    let branches = match selected {
        Some(selected) => design::branches(&state.db, selected.design.id)
            .await?
            .into_iter()
            .map(|branch| branch.name.to_string())
            .collect(),
        None => vec![design::DEFAULT_BRANCH.to_owned()],
    };
    // The branch of the page the form was opened from; the design's
    // default branch when none was named or the name is not the design's.
    let branch = match &opened.branch {
        Some(branch) if branches.contains(branch) || branch.is_empty() => branch.clone(),
        _ => selected
            .map(|member| member.design.default_branch.clone())
            .filter(|default| branches.contains(default))
            .unwrap_or_else(|| branches[0].clone()),
    };
    let page = UploadPage {
        design: selected.map(Membership::path).unwrap_or_default(),
        designs,
        branches,
        branch,
        from: opened.from.clone(),
        handle: settled.handle,
        error,
        file_mb: Limits::DEFAULT.file_mb(),
        revision_mb: Limits::DEFAULT.revision_mb(),
    };
    Ok(personal(settled.jar, (status, Html(page.render()?))))
}

/// `GET /upload`: the form, opened for the design, branch and revision
/// the query names, if any.
pub(super) async fn upload_form(
    State(state): State<AppState>,
    settled: Settled,
    Query(opened): Query<Opened>,
) -> Result<Response> {
    upload_page(&state, settled, &opened, StatusCode::OK, None).await
}

/// The status a refusal is answered with: too large is 413, a file the
/// server will not take is 422, a conflict with what exists is 409, a
/// design that does not exist is 404, a body that is not multipart is
/// 400.
fn status(refusal: &Refusal) -> StatusCode {
    match refusal {
        Refusal::FileTooLarge { .. }
        | Refusal::RevisionTooLarge { .. }
        | Refusal::AccountFull { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        Refusal::NoKicadFile
        | Refusal::DoesNotParse { .. }
        | Refusal::ParseTimeout { .. }
        | Refusal::Blocked { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        Refusal::SlugTaken { .. }
        | Refusal::SlugMoved { .. }
        | Refusal::HeadMoved { .. }
        | Refusal::BranchTaken { .. } => StatusCode::CONFLICT,
        Refusal::NoSuchDesign { .. }
        | Refusal::NoSuchBranch { .. }
        | Refusal::NoSuchRevision { .. } => StatusCode::NOT_FOUND,
        Refusal::Unreadable { .. } | Refusal::BadBranchName { .. } => StatusCode::BAD_REQUEST,
    }
}

/// A multipart failure as a refusal: the body limit's 413 is the
/// revision cap, anything else is a body the server could not read.
fn multipart_refusal(error: &MultipartError) -> Refusal {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        Refusal::RevisionTooLarge {
            limit_mb: Limits::DEFAULT.revision_mb(),
        }
    } else {
        Refusal::Unreadable {
            reason: error.body_text(),
        }
    }
}

/// The same, as the crate's error, for a `map_err`.
fn unreadable(error: &MultipartError) -> Error {
    multipart_refusal(error).into()
}

/// The text fields of the upload form, gathered as the parts arrive.
#[derive(Default)]
struct Fields {
    design: Option<String>,
    name: Option<String>,
    /// The branch selected, or empty for a new one.
    branch: Option<String>,
    /// The new branch's name.
    new_branch: Option<String>,
    /// The revision a new branch starts from.
    from: Option<String>,
    message: Option<String>,
}

impl Fields {
    /// Where the upload goes, as the fields say: a new design without a
    /// path; else the design at the path (`handle/slug`, as the options
    /// carry it), on the branch selected (the default branch when the
    /// form names none), or, with the empty option, on a new branch named
    /// in `new_branch` starting from `from`.
    fn target(&self) -> Result<Target> {
        let (owner, slug) = match self.design.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() => design_path(path).ok_or_else(|| {
                Error::Refused(Refusal::NoSuchDesign {
                    slug: path.to_owned(),
                })
            })?,
            _ => {
                return Ok(Target::New {
                    name: self.name.clone(),
                });
            }
        };
        let branch = match self.branch.as_deref().map(str::trim) {
            None => BranchTarget::Default,
            Some(name) if !name.is_empty() => {
                BranchTarget::Head(name.parse::<BranchName>().map_err(|_| {
                    Error::Refused(Refusal::NoSuchBranch {
                        branch: name.to_owned(),
                    })
                })?)
            }
            Some(_) => {
                let name = self
                    .new_branch
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default();
                let name = name.parse::<BranchName>().map_err(|error| {
                    Error::Refused(Refusal::BadBranchName {
                        reason: if name.is_empty() {
                            "a new branch needs a name".to_owned()
                        } else {
                            error.to_string()
                        },
                    })
                })?;
                let from = self.from.as_deref().map(str::trim).unwrap_or_default();
                let from = from.parse::<RevisionHash>().map_err(|_| {
                    Error::Refused(if from.is_empty() {
                        Refusal::BadBranchName {
                            reason: "a new branch starts at a revision: open this form from the design's page".to_owned(),
                        }
                    } else {
                        Refusal::NoSuchRevision {
                            hash: from.to_owned(),
                        }
                    })
                })?;
                BranchTarget::New { name, from }
            }
        };
        Ok(Target::Existing {
            owner,
            slug,
            branch,
        })
    }

    /// The form as it was filled, for showing it again with a refusal.
    fn opened(&self) -> Opened {
        Opened {
            design: self.design.clone().unwrap_or_default(),
            branch: self.branch.clone(),
            from: self.from.clone().unwrap_or_default(),
        }
    }
}

/// Read every part into `incoming` and `fields`; a refusal is the
/// outcome when one is met.
async fn receive(
    multipart: &mut Multipart,
    incoming: &mut Incoming,
    fields: &mut Fields,
) -> Result<()> {
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(()),
            Err(error) => return Err(unreadable(&error)),
        };
        match field.name() {
            Some("design") => fields.design = Some(text(field).await?),
            Some("name") => fields.name = Some(text(field).await?),
            Some("branch") => fields.branch = Some(text(field).await?),
            Some("new_branch") => fields.new_branch = Some(text(field).await?),
            Some("from") => fields.from = Some(text(field).await?),
            Some("message") => fields.message = Some(text(field).await?),
            Some("files") => receive_file(field, incoming).await?,
            _ => drain(field).await?,
        }
    }
}

async fn text(field: Field<'_>) -> Result<String> {
    field.text().await.map_err(|error| unreadable(&error))
}

/// A part the upload does not keep is read to its end and forgotten.
async fn drain(mut field: Field<'_>) -> Result<()> {
    while let Some(_chunk) = field.chunk().await.map_err(|error| unreadable(&error))? {}
    Ok(())
}

/// One file part into `incoming`, chunk by chunk, or dropped when it is
/// not a KiCad file (or the empty part a form sends with no file chosen).
async fn receive_file(mut field: Field<'_>, incoming: &mut Incoming) -> Result<()> {
    let Some((path, kind)) = field
        .file_name()
        .filter(|name| !name.is_empty())
        .and_then(Incoming::accepts)
    else {
        return drain(field).await;
    };
    incoming.begin(path, kind).await?;
    while let Some(chunk) = field.chunk().await.map_err(|error| unreadable(&error))? {
        incoming.write(&chunk).await?;
    }
    incoming.end().await
}

/// `POST /upload`: the files become a revision and the browser goes to
/// the design's page. A refusal comes back as the form with the reason.
pub(super) async fn upload(
    State(state): State<AppState>,
    settled: Settled,
    mut multipart: Multipart,
) -> Result<Response> {
    let mut incoming = Incoming::new(Limits::DEFAULT)?;
    let mut fields = Fields::default();
    let received = receive(&mut multipart, &mut incoming, &mut fields).await;
    let author = Author {
        account_id: settled.account_id,
        handle: settled.handle.clone(),
    };
    let target = received.and_then(|()| fields.target());
    // The design's URL is under its owner's handle, which is the author's
    // only for a new design: a collaborator's push lands on the owner's
    // page.
    let owner = match &target {
        Ok(Target::Existing { owner, .. }) => owner.clone(),
        _ => author.handle.clone(),
    };
    let committed = match target {
        Ok(target) => {
            upload::push(
                &state.db,
                state.blobs.as_ref(),
                &Limits::DEFAULT,
                &author,
                target,
                fields.message.as_deref(),
                incoming,
            )
            .await
        }
        Err(error) => Err(error),
    };
    match committed {
        Ok(committed) => {
            let next = history::branch_url(&owner, &committed.design, &committed.branch);
            Ok(personal(settled.jar, Redirect::to(&next)))
        }
        Err(Error::Refused(refusal)) => {
            tracing::info!(%refusal, "upload refused");
            upload_page(
                &state,
                settled,
                &fields.opened(),
                status(&refusal),
                Some(refusal.to_string()),
            )
            .await
        }
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, "upload refused");
            upload_page(
                &state,
                settled,
                &fields.opened(),
                StatusCode::FORBIDDEN,
                Some(error.to_string()),
            )
            .await
        }
        Err(error) => Err(error),
    }
}

/// The design the form's `design` field names, `handle/slug` as the
/// options carry it, or `None` for text that is not a design's path.
fn design_path(path: &str) -> Option<(Handle, Slug)> {
    let (owner, slug) = path.split_once('/')?;
    Some((owner.parse().ok()?, slug.parse().ok()?))
}
