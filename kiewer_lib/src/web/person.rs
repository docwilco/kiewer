//! A person's page: `/{handle}` lists their public designs. The same for
//! everyone, so it stays cacheable (ADR 0003): nothing here reads the
//! session, and the person's own unlisted designs are not shown even to
//! them (the upload form lists those).

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Html, Response};

use super::AppState;
use crate::account::Handle;
use crate::cache::{self, Key};
use crate::error::Result;
use crate::listing::{self, Listed};

#[derive(Template)]
#[template(path = "person.html")]
struct PersonPage {
    handle: Handle,
    /// The person's public designs, the most recently pushed first.
    designs: Vec<Listed>,
}

/// `GET /{handle}`: the person's public designs, or 404 for a segment
/// that is not a handle or a handle nobody has. The static routes
/// (`/about`, `/upload`, every reserved word) win over this one: axum
/// matches the most specific route, and `account::RESERVED` keeps a
/// handle from ever being one of them.
pub(super) async fn page(
    State(state): State<AppState>,
    Path(segment): Path<String>,
) -> Result<Response> {
    let not_found = || cache::not_found_under(&segment, "no such person\n");
    let Ok(handle) = segment.parse::<Handle>() else {
        return Ok(not_found());
    };
    let Some(account) = listing::account_at(&state.db, handle.as_str()).await? else {
        return Ok(not_found());
    };
    let keys = [Key::Handle(handle.clone())];
    let page = PersonPage {
        designs: listing::public_of(&state.db, account).await?,
        handle,
    };
    Ok(cache::page(&keys, Html(page.render()?)))
}
