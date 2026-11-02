//! The settings pages: the account's sign-in methods with a link for each
//! registered provider not yet among them, and its sessions, each
//! revocable and all the others at once. Every page here is the signed-in
//! person's own, so every response carries `private, no-store` (ADR 0003:
//! settings pages are never shared-cached).

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::auth::{Settled, personal, session_cookie_removal};
use crate::account::{self, Handle, Identity, RESERVED_DAYS};
use crate::auth::Provider;
use crate::delete;
use crate::error::Result;
use crate::session::{self, ListedSession, SessionKey};

#[derive(Template)]
#[template(path = "settings.html")]
struct AccountPage {
    handle: Handle,
    /// The identities that sign in to the account, oldest first.
    identities: Vec<Identity>,
    /// Registered providers with no identity on the account yet: one link
    /// offer each.
    linkable: Vec<Provider>,
    /// The provider whose identity was just refused because it signs in
    /// to another account, from the callback's redirect.
    refused: Option<Provider>,
    /// The names of the person's designs with other people on them,
    /// which an account delete takes from those people too.
    shared: Vec<String>,
    /// How long the handle stays reserved after the delete.
    reserved_days: i32,
}

/// The settings page's query: `?refused=<provider>` after a link the
/// callback could not make.
#[derive(Debug, Deserialize)]
pub struct AccountQuery {
    refused: Option<Provider>,
}

/// `GET /settings`: the account's handle and sign-in methods, and the
/// form that deletes it (`web/delete.rs`).
pub async fn account(
    State(state): State<AppState>,
    Query(query): Query<AccountQuery>,
    settled: Settled,
) -> Result<Response> {
    let identities = account::identities(&state.db, settled.account_id).await?;
    let linkable = state
        .auth
        .providers()
        .into_iter()
        .filter(|provider| {
            !identities
                .iter()
                .any(|identity| identity.provider == *provider)
        })
        .collect();
    let page = AccountPage {
        shared: delete::shared_designs(&state.db, settled.account_id).await?,
        handle: settled.handle,
        identities,
        linkable,
        refused: query.refused,
        reserved_days: RESERVED_DAYS,
    };
    Ok(personal(
        settled.jar,
        (StatusCode::OK, Html(page.render()?)),
    ))
}

#[derive(Template)]
#[template(path = "sessions.html")]
struct SessionsPage {
    /// The account's live sessions, most recently seen first.
    sessions: Vec<ListedSession>,
}

const SESSIONS: &str = "/settings/sessions";

/// `GET /settings/sessions`: when each session started and was last seen,
/// the current one marked, a revoke for each other one and for all of
/// them at once.
pub async fn sessions(State(state): State<AppState>, settled: Settled) -> Result<Response> {
    let page = SessionsPage {
        sessions: session::list(&state.db, settled.account_id, &settled.id).await?,
    };
    Ok(personal(
        settled.jar,
        (StatusCode::OK, Html(page.render()?)),
    ))
}

/// The revoke form's one field: the session's key as the page printed
/// it.
#[derive(Debug, Deserialize)]
pub struct RevokeForm {
    key: String,
}

/// `POST /settings/sessions/revoke`: end one session of the account. A
/// key that is not one is a 400; one that is not this account's revokes
/// nothing and lands on the page like any other. The current session's
/// own key is a sign-out: the cookie goes with the row.
pub async fn revoke(
    State(state): State<AppState>,
    settled: Settled,
    Form(form): Form<RevokeForm>,
) -> Result<Response> {
    let Ok(key) = form.key.parse::<SessionKey>() else {
        return Ok(personal(
            settled.jar,
            (StatusCode::BAD_REQUEST, "that is not a session key\n"),
        ));
    };
    session::revoke(&state.db, settled.account_id, &key).await?;
    if key == settled.id.key() {
        tracing::info!("the current session revoked itself");
        return Ok(personal(
            settled.jar.remove(session_cookie_removal()),
            Redirect::to("/"),
        ));
    }
    Ok(personal(settled.jar, Redirect::to(SESSIONS)))
}

/// `POST /settings/sessions/revoke-others`: end every session of the
/// account but this browser's.
pub async fn revoke_others(State(state): State<AppState>, settled: Settled) -> Result<Response> {
    session::revoke_others(&state.db, settled.account_id, &settled.id).await?;
    Ok(personal(settled.jar, Redirect::to(SESSIONS)))
}
