//! Sign-in, the welcome page, sign-out and `/api/me`: the only handlers
//! that read the session (ADR 0003). Every other page renders the same
//! for everyone and a script asks `/api/me` who is signed in. Every
//! response here depends on the session or the browser, so every one
//! carries `Cache-Control: private, no-store`.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::account::{self, Handle};
use crate::auth::Provider;
use crate::error::{Error, Result};
use crate::session::{self, LIFETIME_DAYS, Session, SessionId};

/// The session cookie: the random id, nothing else.
pub const SESSION_COOKIE: &str = "kiewer_session";

/// The cookie a sign-in under way keeps its state and PKCE verifier in,
/// under `/auth` only, for ten minutes.
const OAUTH_COOKIE: &str = "kiewer_oauth";

/// The value every response here carries: a shared cache never keeps it
/// (ADR 0003).
const NO_STORE: (axum::http::HeaderName, &str) = (CACHE_CONTROL, "private, no-store");

/// The session cookie for `id`, with the full lifetime: set at sign-in
/// and again whenever a use slides the row's expiry, so the browser and
/// the row agree. `secure` follows the public URL's scheme
/// ([`crate::auth::Auth::secure_cookies`]).
fn session_cookie(id: SessionId, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, id.to_string()))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(i64::from(LIFETIME_DAYS)))
        .build()
}

/// The session cookie as a removal: the browser forgets it.
fn session_cookie_removal() -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE).path("/").build()
}

/// The state and the verifier in one cookie, dot-separated: both are
/// base64url, which has no dot, and a dot needs no percent-encoding.
fn oauth_cookie(state: &str, verifier: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((OAUTH_COOKIE, format!("{state}.{verifier}")))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/auth")
        .max_age(time::Duration::minutes(10))
        .build()
}

fn oauth_cookie_removal() -> Cookie<'static> {
    Cookie::build(OAUTH_COOKIE).path("/auth").build()
}

/// The session id the jar's cookie carries, when it carries one.
fn cookie_session_id(jar: &CookieJar) -> Option<SessionId> {
    jar.get(SESSION_COOKIE)
        .and_then(|cookie| cookie.value().parse::<SessionId>().ok())
}

/// The live session behind the jar's cookie, with its id, or `None` when
/// there is no cookie, it is not an id, or the row is gone or expired.
/// The jar comes back with the cookie set again when this use slid the
/// row's expiry, so every handler's response renews the Max-Age.
async fn current(
    state: &AppState,
    jar: CookieJar,
) -> Result<(CookieJar, Option<(SessionId, Session)>)> {
    let Some(id) = cookie_session_id(&jar) else {
        return Ok((jar, None));
    };
    let Some(session) = session::find(&state.db, &id).await? else {
        return Ok((jar, None));
    };
    let jar = if session.refreshed {
        jar.add(session_cookie(id, state.auth.secure_cookies()))
    } else {
        jar
    };
    Ok((jar, Some((id, session))))
}

/// A response that depends on this browser: the jar's changes and
/// `private, no-store`.
fn personal(jar: CookieJar, response: impl IntoResponse) -> Response {
    (jar, [NO_STORE], response).into_response()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, [NO_STORE], format!("{what}\n")).into_response()
}

/// The provider a path segment names; `None` is a 404.
fn provider_named(segment: &str) -> Option<Provider> {
    segment.parse::<Provider>().ok()
}

const NO_SUCH_PROVIDER: &str = "no such sign-in provider";

/// `GET /auth/{provider}`: start a sign-in. The state and verifier go in
/// a cookie, the browser goes to the provider.
pub async fn begin(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    jar: CookieJar,
) -> Response {
    let Some(provider) = provider_named(&provider) else {
        return not_found(NO_SUCH_PROVIDER);
    };
    let Some(start) = state.auth.begin(provider) else {
        return not_found("sign-in with this provider is not configured");
    };
    let jar = jar.add(oauth_cookie(
        &start.state,
        &start.verifier,
        state.auth.secure_cookies(),
    ));
    personal(jar, Redirect::to(&start.authorize_url))
}

/// What the provider sends the browser back with.
#[derive(Debug, Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    /// Set when the person declined or the provider refused.
    error: Option<String>,
}

/// `GET /auth/{provider}/callback`: finish a sign-in. The state has to be
/// the one in this browser's cookie; then the code becomes a token, the
/// token a provider user, the user an account, the account a session. A
/// provider that refuses the code or cannot be reached is a 502 with a
/// line to start again, not a 500: the server is fine.
pub async fn callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<Callback>,
    jar: CookieJar,
) -> Result<Response> {
    let Some(provider) = provider_named(&provider) else {
        return Ok(not_found(NO_SUCH_PROVIDER));
    };
    // Read before the removal: a removed cookie is gone from the jar.
    let stored = jar
        .get(OAUTH_COOKIE)
        .map(|cookie| cookie.value().to_owned())
        .and_then(|value| {
            value
                .split_once('.')
                .map(|(s, v)| (s.to_owned(), v.to_owned()))
        });
    // One sign-in per state cookie, whatever happens next.
    let jar = jar.remove(oauth_cookie_removal());
    if let Some(error) = query.error {
        tracing::info!(%provider, error, "sign-in declined or refused at the provider");
        return Ok(personal(jar, Redirect::to("/")));
    }
    let (code, verifier) = match (query.code, query.state, stored) {
        (Some(code), Some(echoed), Some((expected, verifier))) if echoed == expected => {
            (code, verifier)
        }
        _ => {
            return Ok(personal(
                jar,
                (
                    StatusCode::BAD_REQUEST,
                    "this sign-in did not start in this browser, or took longer than ten minutes; start again\n",
                ),
            ));
        }
    };
    let user = match state.auth.finish(provider, code, verifier).await {
        Ok(Some(user)) => user,
        Ok(None) => return Ok(not_found("sign-in with this provider is not configured")),
        Err(error @ (Error::OAuth { .. } | Error::Http(_))) => {
            tracing::warn!(%provider, %error, "sign-in failed at the provider");
            return Ok(personal(
                jar,
                (
                    StatusCode::BAD_GATEWAY,
                    format!("sign-in with {provider} did not complete; start again\n"),
                ),
            ));
        }
        Err(error) => return Err(error),
    };
    let signed_in = account::sign_in(&state.db, provider, &user).await?;
    let id = session::start(&state.db, signed_in.account_id).await?;
    tracing::info!(%provider, new_account = signed_in.is_new, "signed in");
    let jar = jar.add(session_cookie(id, state.auth.secure_cookies()));
    let next = if signed_in.handle.is_some() {
        "/"
    } else {
        "/welcome"
    };
    Ok(personal(jar, Redirect::to(next)))
}

#[derive(Template)]
#[template(path = "welcome.html")]
struct Welcome {
    /// What the handle field starts with: the provider's login made into
    /// a handle, or the person's own attempt when it was refused.
    handle: String,
    /// Why the last attempt was refused, or none.
    error: Option<String>,
}

fn welcome_page(jar: CookieJar, status: StatusCode, page: &Welcome) -> Result<Response> {
    Ok(personal(jar, (status, Html(page.render()?))))
}

/// The welcome page's guard: the session of a signed-in person who has no
/// handle yet, or the redirect home for anyone else.
async fn without_handle(
    state: &AppState,
    jar: CookieJar,
) -> Result<std::result::Result<(CookieJar, Session), Response>> {
    let (jar, found) = current(state, jar).await?;
    Ok(match found {
        Some((_, session)) if session.handle.is_none() => Ok((jar, session)),
        _ => Err(personal(jar, Redirect::to("/"))),
    })
}

/// `GET /welcome`: the handle form, for a signed-in person without one.
/// Anyone else goes to the front page.
pub async fn welcome(State(state): State<AppState>, jar: CookieJar) -> Result<Response> {
    let (jar, session) = match without_handle(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let suggested = account::suggested_handle(&state.db, session.account_id).await?;
    welcome_page(
        jar,
        StatusCode::OK,
        &Welcome {
            handle: suggested
                .map(|handle| handle.to_string())
                .unwrap_or_default(),
            error: None,
        },
    )
}

/// The welcome form's one field.
#[derive(Debug, Deserialize)]
pub struct HandleForm {
    handle: String,
}

/// `POST /welcome`: the person's choice. A malformed handle comes back as
/// 422 and a taken one as 409, each with the form again and the reason.
pub async fn choose_handle(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<HandleForm>,
) -> Result<Response> {
    let (jar, session) = match without_handle(&state, jar).await? {
        Ok(found) => found,
        Err(response) => return Ok(response),
    };
    let attempt = form.handle.trim().to_owned();
    let refused = |jar: CookieJar, status: StatusCode, reason: String| {
        welcome_page(
            jar,
            status,
            &Welcome {
                handle: attempt.clone(),
                error: Some(reason),
            },
        )
    };
    let handle = match attempt.parse::<Handle>() {
        Ok(handle) => handle,
        Err(Error::Malformed(reason)) => {
            return refused(jar, StatusCode::UNPROCESSABLE_ENTITY, reason);
        }
        Err(error) => return Err(error),
    };
    match account::choose_handle(&state.db, session.account_id, &handle).await {
        Ok(()) => Ok(personal(jar, Redirect::to("/"))),
        Err(Error::HandleTaken(_)) => refused(
            jar,
            StatusCode::CONFLICT,
            format!("handle {attempt:?} is taken; pick another"),
        ),
        Err(error) => Err(error),
    }
}

/// `POST /auth/sign-out`: the session row goes and the browser forgets
/// the cookie. A POST, so a cross-site link cannot sign anyone out; the
/// cookie is `SameSite=Lax`, so a cross-site form does not carry it.
pub async fn sign_out(State(state): State<AppState>, jar: CookieJar) -> Result<Response> {
    if let Some(id) = cookie_session_id(&jar) {
        session::end(&state.db, &id).await?;
    }
    let jar = jar.remove(session_cookie_removal());
    Ok(personal(jar, Redirect::to("/")))
}

/// What `/api/me` says.
#[derive(Debug, Serialize)]
pub struct Me {
    /// Whether a live session is behind the cookie.
    signed_in: bool,
    /// The handle, or null: signed out, or signed in and still to pick one.
    handle: Option<String>,
}

/// `GET /api/me`: who is signed in, never cached. The one request a
/// cacheable page makes that depends on the session.
pub async fn me(State(state): State<AppState>, jar: CookieJar) -> Result<Response> {
    let (jar, found) = current(&state, jar).await?;
    let me = match found {
        Some((_, session)) => Me {
            signed_in: true,
            handle: session.handle.map(|handle| handle.to_string()),
        },
        None => Me {
            signed_in: false,
            handle: None,
        },
    };
    Ok(personal(jar, Json(me)))
}
