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
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use super::AppState;
use super::auth::{personal, session_cookie_removal, settled};
use crate::account::{self, Identity};
use crate::auth::Provider;
use crate::error::Result;
use crate::session::{self, ListedSession, SessionKey};

#[derive(Template)]
#[template(path = "settings.html")]
struct Account {
    handle: String,
    /// The identities that sign in to the account, oldest first.
    identities: Vec<Identity>,
    /// Registered providers with no identity on the account yet: one link
    /// offer each.
    linkable: Vec<Provider>,
    /// The provider whose identity was just refused because it signs in
    /// to another account, from the callback's redirect.
    refused: Option<Provider>,
}

/// The settings page's query: `?refused=<provider>` after a link the
/// callback could not make.
#[derive(Debug, Deserialize)]
pub struct AccountQuery {
    refused: Option<Provider>,
}

/// `GET /settings`: the account's handle and sign-in methods.
pub async fn account(
    State(state): State<AppState>,
    Query(query): Query<AccountQuery>,
    jar: CookieJar,
) -> Result<Response> {
    let (jar, _, session) = match settled(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let identities = account::identities(&state.db, session.account_id).await?;
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
    let page = Account {
        handle: session
            .handle
            .map(|handle| handle.to_string())
            .unwrap_or_default(),
        identities,
        linkable,
        refused: query.refused,
    };
    Ok(personal(jar, (StatusCode::OK, Html(page.render()?))))
}

#[derive(Template)]
#[template(path = "sessions.html")]
struct Sessions {
    /// The account's live sessions, most recently seen first.
    sessions: Vec<ListedSession>,
}

const SESSIONS: &str = "/settings/sessions";

/// `GET /settings/sessions`: when each session started and was last seen,
/// the current one marked, a revoke for each other one and for all of
/// them at once.
pub async fn sessions(State(state): State<AppState>, jar: CookieJar) -> Result<Response> {
    let (jar, id, session) = match settled(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let page = Sessions {
        sessions: session::list(&state.db, session.account_id, &id).await?,
    };
    Ok(personal(jar, (StatusCode::OK, Html(page.render()?))))
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
    jar: CookieJar,
    Form(form): Form<RevokeForm>,
) -> Result<Response> {
    let (jar, id, session) = match settled(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let Ok(key) = form.key.parse::<SessionKey>() else {
        return Ok(personal(
            jar,
            (StatusCode::BAD_REQUEST, "that is not a session key\n"),
        ));
    };
    session::revoke(&state.db, session.account_id, &key).await?;
    if key == id.key() {
        tracing::info!("the current session revoked itself");
        return Ok(personal(
            jar.remove(session_cookie_removal()),
            Redirect::to("/"),
        ));
    }
    Ok(personal(jar, Redirect::to(SESSIONS)))
}

/// `POST /settings/sessions/revoke-others`: end every session of the
/// account but this browser's.
pub async fn revoke_others(State(state): State<AppState>, jar: CookieJar) -> Result<Response> {
    let (jar, id, session) = match settled(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    session::revoke_others(&state.db, session.account_id, &id).await?;
    Ok(personal(jar, Redirect::to(SESSIONS)))
}
