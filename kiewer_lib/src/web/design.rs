//! The design page and the upload: `/{handle}/{slug}` shows the head of
//! the design's default branch in the viewer with the revision's details
//! (`web/viewer.rs`'s page); `/upload` is the form and its POST for a
//! signed-in person with a handle, streamed part by part into
//! `upload::Incoming` and committed by `upload::push`. The upload pages
//! are the person's own, so they carry `private, no-store`; the design
//! page renders the same for everyone.

use askama::Template;
use axum::extract::multipart::{Field, Multipart, MultipartError};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::LOCATION;
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::auth::{Settled, personal};
use super::design_settings::SettingsView;
use super::viewer::{DesignView, FileView, Page};
use super::{AppState, blob, schematic};
use crate::account::Handle;
use crate::description::{self, Description};
use crate::design::{self, DesignHead, Slug};
use crate::error::{Error, Refusal, Result};
use crate::roles::{self, Membership};
use crate::slug;
use crate::upload::{self, Author, Incoming, Limits, Target};

/// The URL of a design's page.
fn design_url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}")
}

/// The URL of a design's people page.
pub(super) fn people_url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/people")
}

/// The design page for `shown`: the viewer at the head with the
/// revision's details, the design's `description`, and `settings` as
/// its settings block. Rendered here for the page and for the settings
/// form's refusal, which shows the page again with the reason.
pub(super) fn design_page(
    shown: &DesignHead,
    description: &Description,
    settings: SettingsView,
) -> Result<String> {
    let board_url = shown.board().map(|entry| blob::url(&entry.blob));
    let schematic_url = shown
        .schematic()
        .map(|_| schematic::url(&shown.owner, &shown.design.slug));
    let view = DesignView {
        handle: shown.owner.to_string(),
        slug: shown.design.slug.to_string(),
        branch: shown.branch.clone(),
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
    let not_found = || (StatusCode::NOT_FOUND, "no such design\n").into_response();
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found());
    };
    let Some(shown) = design::show(&state.db, handle.as_str(), slug.as_str()).await? else {
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
    let description = description::of(&state.db, shown.design.id).await?;
    let settings = SettingsView::of(&handle, &shown.design, &description);
    Ok(Html(design_page(&shown, &description, settings)?).into_response())
}

#[derive(Template)]
#[template(path = "upload.html")]
struct UploadPage {
    handle: Handle,
    /// The designs the person may push to, newest first: the targets
    /// besides a new one.
    designs: Vec<Membership>,
    /// Why the last attempt was refused, or none.
    error: Option<String>,
    file_mb: u64,
    revision_mb: u64,
}

async fn upload_page(
    state: &AppState,
    settled: Settled,
    status: StatusCode,
    error: Option<String>,
) -> Result<Response> {
    let page = UploadPage {
        designs: roles::designs_of(&state.db, settled.account_id).await?,
        handle: settled.handle,
        error,
        file_mb: Limits::DEFAULT.file_mb(),
        revision_mb: Limits::DEFAULT.revision_mb(),
    };
    Ok(personal(settled.jar, (status, Html(page.render()?))))
}

/// `GET /upload`: the form.
pub(super) async fn upload_form(
    State(state): State<AppState>,
    settled: Settled,
) -> Result<Response> {
    upload_page(&state, settled, StatusCode::OK, None).await
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
        Refusal::SlugTaken { .. } | Refusal::SlugMoved { .. } | Refusal::HeadMoved { .. } => {
            StatusCode::CONFLICT
        }
        Refusal::NoSuchDesign { .. } => StatusCode::NOT_FOUND,
        Refusal::Unreadable { .. } => StatusCode::BAD_REQUEST,
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
    message: Option<String>,
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
    let target = match fields.design.as_deref().map(str::trim) {
        Some(path) if !path.is_empty() => match existing(path) {
            Some(target) => target,
            None => {
                return upload_page(
                    &state,
                    settled,
                    StatusCode::NOT_FOUND,
                    Some(
                        Refusal::NoSuchDesign {
                            slug: path.to_owned(),
                        }
                        .to_string(),
                    ),
                )
                .await;
            }
        },
        _ => Target::New { name: fields.name },
    };
    let author = Author {
        account_id: settled.account_id,
        handle: settled.handle.clone(),
    };
    // The design's URL is under its owner's handle, which is the author's
    // only for a new design: a collaborator's push lands on the owner's
    // page.
    let owner = match &target {
        Target::Existing { owner, .. } => owner.clone(),
        Target::New { .. } => author.handle.clone(),
    };
    let committed = match received {
        Ok(()) => {
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
            let next = design_url(&owner, &committed.design.slug);
            Ok(personal(settled.jar, Redirect::to(&next)))
        }
        Err(Error::Refused(refusal)) => {
            tracing::info!(%refusal, "upload refused");
            upload_page(&state, settled, status(&refusal), Some(refusal.to_string())).await
        }
        Err(error @ Error::Forbidden { .. }) => {
            tracing::info!(%error, "upload refused");
            upload_page(
                &state,
                settled,
                StatusCode::FORBIDDEN,
                Some(error.to_string()),
            )
            .await
        }
        Err(error) => Err(error),
    }
}

/// The target the form's `design` field names, `handle/slug` as the
/// options carry it, or `None` for text that is not a design's path.
fn existing(path: &str) -> Option<Target> {
    let (owner, slug) = path.split_once('/')?;
    Some(Target::Existing {
        owner: owner.parse().ok()?,
        slug: slug.parse().ok()?,
    })
}
