//! The settings pages: the account's sign-in methods with a link for each
//! registered provider not yet among them. Every page here is the
//! signed-in person's own, so every response carries `private, no-store`
//! (ADR 0003: settings pages are never shared-cached).

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, Response};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;

use super::AppState;
use super::auth::{personal, settled};
use crate::account::{self, Identity};
use crate::auth::Provider;
use crate::error::Result;

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
