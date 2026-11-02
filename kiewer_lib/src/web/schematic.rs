//! The schematic page: one sheet in the viewer. At `/schematic` the sheet
//! is the hand-off's at a fixed URL, as `/viewer` shows the hand-off
//! board; at `/{handle}/{slug}/schematic` it is the root sheet of the
//! design's head revision, fetched from its blob. The page loads the same
//! wasm as the board page (`web/viewer.rs`) and calls `load_schematic`.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use super::viewer::{NOT_BUILT, embedded};
use super::{AppState, blob};
use crate::account::Handle;
use crate::design::{self, Slug};
use crate::error::Result;

/// The schematic routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/schematic", get(hand_off))
        .route(SCHEMATIC_PATH, get(schematic))
        .route("/{handle}/{slug}/schematic", get(page))
}

/// Where the hand-off page fetches its sheet from.
pub const SCHEMATIC_PATH: &str = "/schematic/midi8knob.kicad_sch";

/// The hand-off schematic, in the binary.
const SCHEMATIC: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_sch");

/// The URL of a design's schematic page.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/schematic")
}

/// The schematic page.
#[derive(Template)]
#[template(path = "schematic.html")]
pub struct Page {
    pub build: String,
    pub embedded: bool,
    pub not_built: &'static str,
    /// What the tab and the header name.
    pub title: String,
    /// Where the page fetches the sheet from, or `None` for a revision
    /// without a schematic.
    pub schematic_url: Option<String>,
    /// The design's board page, when there is one to go back to.
    pub design_url: Option<String>,
}

impl Page {
    /// A page for `title` with the viewer's build state filled in.
    #[must_use]
    pub fn new(title: String, schematic_url: Option<String>, design_url: Option<String>) -> Self {
        Self {
            build: crate::build::line(),
            embedded: embedded(),
            not_built: NOT_BUILT.trim_end(),
            title,
            schematic_url,
            design_url,
        }
    }
}

async fn hand_off() -> Result<Html<String>> {
    let page = Page::new(
        "midi8knob".to_owned(),
        Some(SCHEMATIC_PATH.to_owned()),
        None,
    );
    Ok(Html(page.render()?))
}

async fn schematic() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], SCHEMATIC)
}

/// `GET /{handle}/{slug}/schematic`: the root sheet of the head of the
/// design's default branch, or 404 when there is no such design, it has
/// no revision yet, or the revision holds no schematic.
async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
) -> Result<Response> {
    let not_found = || (StatusCode::NOT_FOUND, "no such schematic\n").into_response();
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found());
    };
    let Some(shown) = design::show(&state.db, handle.as_str(), slug.as_str()).await? else {
        return Ok(not_found());
    };
    let Some(sheet_url) = shown.schematic().map(|sheet| blob::url(&sheet.blob)) else {
        return Ok(not_found());
    };
    let design_url = format!("/{}/{}", shown.owner, shown.design.slug);
    let page = Page::new(shown.design.name, Some(sheet_url), Some(design_url));
    Ok(Html(page.render()?).into_response())
}
