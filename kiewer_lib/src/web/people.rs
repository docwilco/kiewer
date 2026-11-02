//! The people page (GLOSSARY): `/{handle}/{slug}/people` lists who has
//! which role on a design and, for an admin or the owner, changes it;
//! the owner alone transfers the design and sets its visibility. Every
//! page here depends on the session (what it offers follows the role),
//! so every response carries `private, no-store` (ADR 0003); the design
//! page stays the same for everyone and links here.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal};
use super::design::people_url;
use crate::account::Handle;
use crate::design::{self, Design, Slug};
use crate::error::{Error, Result};
use crate::roles::{self, Changed, Role, Visibility};

/// The people page's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{handle}/{slug}/people", get(page))
        .route("/{handle}/{slug}/people/role", post(set_role))
        .route("/{handle}/{slug}/people/remove", post(remove))
        .route("/{handle}/{slug}/people/transfer", post(transfer))
        .route("/{handle}/{slug}/people/visibility", post(set_visibility))
}

/// One person as the page lists them.
struct PersonView {
    handle: String,
    role: String,
    /// The owner's row has no form: a transfer is how that changes.
    is_owner: bool,
    /// The grantable roles as `(name, selected)`, for the row's select.
    options: Vec<(String, bool)>,
}

#[derive(Template)]
#[template(path = "people.html")]
struct PeoplePage {
    /// The signed-in person's handle.
    handle: Handle,
    owner: Handle,
    slug: Slug,
    name: String,
    /// The page's own URL, which the forms post under.
    base: String,
    /// Whether the signed-in person is the owner: the transfer and
    /// visibility forms are theirs alone.
    is_owner: bool,
    people: Vec<PersonView>,
    /// The grantable roles as `(name, selected)` for the add form, none
    /// selected.
    grantable: Vec<(String, bool)>,
    /// Both visibilities as `(name, selected)`.
    visibilities: Vec<(String, bool)>,
    /// Why the last change was refused, or none.
    error: Option<String>,
}

/// A signed-in person with at least `at_least` on the design at
/// `/{handle}/{slug}`, with the design, or the response that says why
/// not: 404 for no such design, 403 for a role below the one needed.
struct Access {
    settled: Settled,
    design: Design,
    owner: Handle,
    role: Role,
}

async fn access(
    state: &AppState,
    settled: Settled,
    handle: &str,
    slug: &str,
    at_least: Role,
) -> Result<std::result::Result<Access, Response>> {
    let not_found = |settled: Settled| {
        Ok(Err(personal(
            settled.jar,
            (StatusCode::NOT_FOUND, "no such design\n"),
        )))
    };
    let (Ok(owner), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return not_found(settled);
    };
    // Two reads on the pool for one page: the design, then the role. A
    // transfer between them is seen by the role read, which is the one
    // that decides.
    let mut conn = state.db.acquire().await?;
    let Some(design) = design::find_at(&mut conn, &owner, &slug).await? else {
        return not_found(settled);
    };
    drop(conn);
    match roles::require(&state.db, design.id, settled.account_id, at_least).await {
        Ok(role) => Ok(Ok(Access {
            settled,
            design,
            owner,
            role,
        })),
        Err(error @ Error::Forbidden { .. }) => Ok(Err(personal(
            settled.jar,
            (StatusCode::FORBIDDEN, format!("{error}\n")),
        ))),
        Err(error) => Err(error),
    }
}

/// The page for `access`, at `status`, with `error` when a change was
/// refused.
async fn render(
    state: &AppState,
    access: Access,
    status: StatusCode,
    error: Option<String>,
) -> Result<Response> {
    let options = |selected: Option<Role>| {
        Role::GRANTABLE
            .into_iter()
            .map(|role| (role.to_string(), Some(role) == selected))
            .collect::<Vec<_>>()
    };
    let people = roles::people(&state.db, access.design.id)
        .await?
        .into_iter()
        .map(|person| PersonView {
            handle: person.handle.to_string(),
            role: person.role.to_string(),
            is_owner: person.role == Role::Owner,
            options: options(Some(person.role)),
        })
        .collect();
    let page = PeoplePage {
        handle: access.settled.handle,
        base: people_url(&access.owner, &access.design.slug),
        owner: access.owner,
        slug: access.design.slug,
        name: access.design.name,
        is_owner: access.role == Role::Owner,
        people,
        grantable: options(None),
        visibilities: Visibility::ALL
            .into_iter()
            .map(|visibility| {
                (
                    visibility.to_string(),
                    visibility == access.design.visibility,
                )
            })
            .collect(),
        error,
    };
    Ok(personal(access.settled.jar, (status, Html(page.render()?))))
}

/// `GET /{handle}/{slug}/people`: the list and the forms, for an admin
/// or the owner.
async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
) -> Result<Response> {
    let access = match access(&state, settled, &handle, &slug, Role::Admin).await? {
        Ok(access) => access,
        Err(refused) => return Ok(refused),
    };
    render(&state, access, StatusCode::OK, None).await
}

/// What a refused change comes back as: the page again with the reason,
/// at the status the reason has (404 for no such person, 409 for the
/// owner or a taken slug, 422 for a field that is not what it should be).
async fn refused(
    state: &AppState,
    access: Access,
    status: StatusCode,
    reason: String,
) -> Result<Response> {
    render(state, access, status, Some(reason)).await
}

/// Back to the page after a change.
fn done(access: Access) -> Response {
    personal(
        access.settled.jar,
        Redirect::to(&people_url(&access.owner, &access.design.slug)),
    )
}

/// The outcome of a change to a person as a response: the page again or
/// the refusal naming `handle`.
async fn after(
    state: &AppState,
    access: Access,
    handle: &Handle,
    changed: Changed,
) -> Result<Response> {
    match changed {
        Changed::Done => Ok(done(access)),
        Changed::NoSuchPerson => {
            refused(
                state,
                access,
                StatusCode::NOT_FOUND,
                format!("there is no one with the handle {handle}"),
            )
            .await
        }
        Changed::TheOwner => {
            refused(
                state,
                access,
                StatusCode::CONFLICT,
                format!("{handle} is the owner; a transfer is how that changes"),
            )
            .await
        }
        Changed::SlugTaken => {
            let reason = format!(
                "{handle} already has a design at {}; rename this one first",
                access.design.slug
            );
            refused(state, access, StatusCode::CONFLICT, reason).await
        }
    }
}

/// A form that names a person.
#[derive(Debug, Deserialize)]
pub struct PersonForm {
    handle: String,
    /// The role to give, on the role form; absent on the others.
    role: Option<String>,
}

/// The handle a form named, or the 422 for text that is not one.
async fn handle_named(
    state: &AppState,
    access: Access,
    text: &str,
) -> Result<std::result::Result<(Access, Handle), Response>> {
    match text.trim().parse::<Handle>() {
        Ok(handle) => Ok(Ok((access, handle))),
        Err(Error::Malformed(reason)) => Ok(Err(refused(
            state,
            access,
            StatusCode::UNPROCESSABLE_ENTITY,
            reason,
        )
        .await?)),
        Err(error) => Err(error),
    }
}

/// `POST /{handle}/{slug}/people/role`: give `handle` the role, or change
/// the one they have. For an admin or the owner; the owner's own role is
/// refused here, since a transfer is how that moves.
async fn set_role(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<PersonForm>,
) -> Result<Response> {
    let access = match access(&state, settled, &handle, &slug, Role::Admin).await? {
        Ok(access) => access,
        Err(refused) => return Ok(refused),
    };
    let (access, person) = match handle_named(&state, access, &form.handle).await? {
        Ok(found) => found,
        Err(refused) => return Ok(refused),
    };
    let role = form
        .role
        .as_deref()
        .and_then(|text| text.parse::<Role>().ok())
        .filter(|role| Role::GRANTABLE.contains(role));
    let Some(role) = role else {
        return refused(
            &state,
            access,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the role is collaborator or admin".to_owned(),
        )
        .await;
    };
    let changed = roles::set_role(&state.db, access.design.id, &person, role).await?;
    tracing::info!(design = %access.design.slug, %person, %role, ?changed, "role set");
    after(&state, access, &person, changed).await
}

/// `POST /{handle}/{slug}/people/remove`: take `handle`'s role away. For
/// an admin or the owner; the owner is never removed.
async fn remove(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<PersonForm>,
) -> Result<Response> {
    let access = match access(&state, settled, &handle, &slug, Role::Admin).await? {
        Ok(access) => access,
        Err(refused) => return Ok(refused),
    };
    let (access, person) = match handle_named(&state, access, &form.handle).await? {
        Ok(found) => found,
        Err(refused) => return Ok(refused),
    };
    let changed = roles::remove_role(&state.db, access.design.id, &person).await?;
    tracing::info!(design = %access.design.slug, %person, ?changed, "role removed");
    after(&state, access, &person, changed).await
}

/// `POST /{handle}/{slug}/people/transfer`: `handle` becomes the owner
/// and the previous owner an admin. For the owner alone. The design's
/// URL moves with it, so the browser goes to the new people page.
async fn transfer(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<PersonForm>,
) -> Result<Response> {
    let access = match access(&state, settled, &handle, &slug, Role::Owner).await? {
        Ok(access) => access,
        Err(refused) => return Ok(refused),
    };
    let (access, person) = match handle_named(&state, access, &form.handle).await? {
        Ok(found) => found,
        Err(refused) => return Ok(refused),
    };
    let changed = roles::transfer(&state.db, access.design.id, &person).await?;
    tracing::info!(design = %access.design.slug, to = %person, ?changed, "transfer");
    match changed {
        Changed::Done => Ok(personal(
            access.settled.jar,
            Redirect::to(&people_url(&person, &access.design.slug)),
        )),
        // The person named is the owner already: nothing to move.
        Changed::TheOwner => Ok(done(access)),
        other => after(&state, access, &person, other).await,
    }
}

/// The visibility form's one field.
#[derive(Debug, Deserialize)]
pub struct VisibilityForm {
    visibility: String,
}

/// `POST /{handle}/{slug}/people/visibility`: public or unlisted. For
/// the owner alone.
async fn set_visibility(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
    settled: Settled,
    Form(form): Form<VisibilityForm>,
) -> Result<Response> {
    let access = match access(&state, settled, &handle, &slug, Role::Owner).await? {
        Ok(access) => access,
        Err(refused) => return Ok(refused),
    };
    let Ok(visibility) = form.visibility.parse::<Visibility>() else {
        return refused(
            &state,
            access,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the visibility is public or unlisted".to_owned(),
        )
        .await;
    };
    roles::set_visibility(&state.db, access.design.id, visibility).await?;
    tracing::info!(design = %access.design.slug, %visibility, "visibility set");
    Ok(done(access))
}
