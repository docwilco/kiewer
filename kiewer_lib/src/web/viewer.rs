//! The viewer page and what it loads: the wasm-bindgen output of
//! `kiewer_viewer` (embedded at build time, see `build.rs`) and the board.
//! At `/viewer` the board is the hand-off one at a fixed URL, kept until
//! a design exists; a design page (`web/design.rs`, `web/history.rs`)
//! renders the same [`Page`] with the revision's board blob and a
//! [`DesignView`].
//!
//! The script and the wasm are static assets under the caching policy
//! (`crate::cache`): each is served at `/viewer/<name>.<hash>.<ext>`,
//! the name `build.rs` gave it from its bytes, as immutable for a year,
//! in the brotli or gzip variant built then when the browser takes it.
//! A page names the script by [`script_url`]; the script fetches the
//! wasm by its hashed name.

use askama::Template;
use axum::Router;
use axum::extract::Path;
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use super::AppState;
use super::open_graph::OpenGraph;
use crate::cache::{Encoding, Policy};
use crate::error::Result;
use crate::roles::Visibility;

/// The viewer's routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/viewer", get(page))
        .route("/viewer/{name}", get(asset))
        .route(BOARD_PATH, get(board))
}

/// Where the page fetches the board from.
pub const BOARD_PATH: &str = "/viewer/midi8knob.kicad_pcb";

/// The hand-off board, in the binary: the one design the tracer shows.
const BOARD: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_pcb");

/// One embedded file in its three codings.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// The hashed file name the route serves it under.
    pub name: &'static str,
    pub content_type: &'static str,
    identity: &'static [u8],
    brotli: &'static [u8],
    gzip: &'static [u8],
}

impl Asset {
    /// The bytes in `encoding`.
    #[must_use]
    pub fn bytes(&self, encoding: Encoding) -> &'static [u8] {
        match encoding {
            Encoding::Brotli => self.brotli,
            Encoding::Gzip => self.gzip,
            Encoding::Identity => self.identity,
        }
    }

    /// The route the asset is at.
    #[must_use]
    pub fn url(&self) -> String {
        format!("/viewer/{}", self.name)
    }
}

/// What the script's route answers with: wasm-bindgen's ES module.
const SCRIPT_TYPE: &str = "text/javascript; charset=utf-8";
/// What the wasm's route answers with: what
/// `WebAssembly.instantiateStreaming` insists on.
const WASM_TYPE: &str = "application/wasm";

/// The embedded assets, when the build found the viewer.
#[cfg(kiewer_viewer_embedded)]
pub const ASSETS: &[Asset] = &[
    Asset {
        name: env!("KIEWER_VIEWER_JS_NAME"),
        content_type: SCRIPT_TYPE,
        identity: include_bytes!(env!("KIEWER_VIEWER_JS")),
        brotli: include_bytes!(env!("KIEWER_VIEWER_JS_BR")),
        gzip: include_bytes!(env!("KIEWER_VIEWER_JS_GZ")),
    },
    Asset {
        name: env!("KIEWER_VIEWER_WASM_NAME"),
        content_type: WASM_TYPE,
        identity: include_bytes!(env!("KIEWER_VIEWER_WASM")),
        brotli: include_bytes!(env!("KIEWER_VIEWER_WASM_BR")),
        gzip: include_bytes!(env!("KIEWER_VIEWER_WASM_GZ")),
    },
];
#[cfg(not(kiewer_viewer_embedded))]
pub const ASSETS: &[Asset] = &[];

/// Whether this build carries the viewer. `false` on a tree where
/// `scripts/build-viewer.sh` has not run: the page then says so and the
/// asset routes answer 503.
#[must_use]
pub fn embedded() -> bool {
    !ASSETS.is_empty()
}

/// The embedded asset of `content_type`, when the build carries one.
fn asset_of(content_type: &str) -> Option<&'static Asset> {
    ASSETS
        .iter()
        .find(|asset| asset.content_type == content_type)
}

/// The URL a page imports the viewer's script from: the hashed name
/// when the build carries it, else the plain one, whose route says the
/// viewer is not built in.
#[must_use]
pub fn script_url() -> String {
    asset_of(SCRIPT_TYPE).map_or_else(|| "/viewer/kiewer_viewer.js".to_owned(), Asset::url)
}

/// The URL the script fetches the wasm from.
#[must_use]
pub fn wasm_url() -> String {
    asset_of(WASM_TYPE).map_or_else(|| "/viewer/kiewer_viewer_bg.wasm".to_owned(), Asset::url)
}

/// What the asset routes say when the viewer is not in the binary.
pub const NOT_BUILT: &str =
    "the viewer is not built into this server: run scripts/build-viewer.sh, then build again\n";

/// One file of the revision a design page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    /// The path in the tree.
    pub path: String,
    /// The file name alone, for the download attribute.
    pub name: String,
    /// Where it downloads: the file route under the revision
    /// (`web/files.rs`).
    pub url: String,
    /// Uncompressed bytes.
    pub size: u64,
}

/// One branch in a design page's branch dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchView {
    pub name: String,
    /// The URL of its head.
    pub url: String,
}

/// One revision in a design page's revision dropdown: the last ten along
/// the branch in context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentView {
    pub ordinal: u32,
    pub short: String,
    pub url: String,
    /// When, UTC, ISO 8601 to the second.
    pub created_at: String,
    /// Whether it is the revision the page shows.
    pub current: bool,
}

/// What a design page says about the revision it shows, folded under the
/// stage. Labels and values are text already: the template knows nothing
/// of the metadata's shape, so a new field is a new pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignView {
    pub handle: String,
    pub slug: String,
    /// The branch in context.
    pub branch: String,
    /// Every branch of the design, by name, with the URL of its head.
    pub branches: Vec<BranchView>,
    /// The last ten revisions along the branch in context, newest first.
    pub recent: Vec<RecentView>,
    /// The revision's ordinal along the branch in context.
    pub ordinal: u32,
    /// How many revisions behind the branch's head the page is: 0 at the
    /// head, else the banner shows.
    pub behind: u32,
    /// The URL of the branch's head.
    pub head_url: String,
    /// The URL of the branch's history page.
    pub history_url: String,
    /// The upload form, opened for this branch at this revision.
    pub upload_url: String,
    /// The upload form, opened for a new branch at this revision.
    pub new_branch_url: String,
    /// The revision's hash, 64 hex characters.
    pub revision: String,
    /// Its short form, the first 8.
    pub short: String,
    /// The handle of the account that uploaded it.
    pub author: String,
    /// When, UTC, ISO 8601 to the second.
    pub created_at: String,
    pub message: Option<String>,
    /// The parsed metadata as label and value pairs.
    pub details: Vec<(&'static str, String)>,
    pub files: Vec<FileView>,
    /// The revision's schematic page, when it holds a sheet.
    pub schematic_url: Option<String>,
    /// Public or unlisted, as the summary says it. The page is the same
    /// for everyone (ADR 0003), so it shows the state and the people page
    /// holds the change.
    pub visibility: Visibility,
    /// The design's people page.
    pub people_url: String,
    /// The description as `crate::description` rendered it: HTML the
    /// template writes as it is, empty for a design without one.
    pub description_html: String,
    /// The settings block: the form's values and a refusal's reason
    /// (`web/design_settings.rs`).
    pub settings: super::design_settings::SettingsView,
    /// The fork form: where it posts and the branch choice
    /// (`web/fork.rs`).
    pub fork: super::fork::ForkView,
    /// The forked-from line, for a design that is a fork.
    pub forked_from: Option<super::fork::ForkedFromView>,
}

/// The viewer page: the hand-off board at `/viewer`, a design's revision
/// at `/{handle}/{slug}` (`web/design.rs`).
#[derive(Template)]
#[template(path = "viewer.html")]
pub struct Page {
    pub build: String,
    pub embedded: bool,
    pub not_built: &'static str,
    /// Where the page imports the viewer's script from ([`script_url`]).
    pub script_url: String,
    /// What the tab and the header name.
    pub title: String,
    /// Where the page fetches the board from, or `None` for a revision
    /// without a board.
    pub board_url: Option<String>,
    /// The design and revision shown, or `None` for the hand-off board.
    pub design: Option<DesignView>,
    /// The Open Graph tags in the head (`web/open_graph.rs`), or `None`
    /// for the hand-off board, which nobody links.
    pub og: Option<OpenGraph>,
}

impl Page {
    /// A page for `title` with the viewer's build state filled in.
    #[must_use]
    pub fn new(
        title: String,
        board_url: Option<String>,
        design: Option<DesignView>,
        og: Option<OpenGraph>,
    ) -> Self {
        Self {
            build: crate::build::line(),
            embedded: embedded(),
            not_built: NOT_BUILT.trim_end(),
            script_url: script_url(),
            title,
            board_url,
            design,
            og,
        }
    }
}

async fn page() -> Result<Html<String>> {
    let page = Page::new(
        "midi8knob".to_owned(),
        Some(BOARD_PATH.to_owned()),
        None,
        None,
    );
    Ok(Html(page.render()?))
}

async fn board() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], BOARD)
}

/// `GET /viewer/{name}`: the embedded asset of that hashed name, in the
/// best coding the request takes, immutable; 404 for a name no asset
/// has, and 503 naming the build script for the two plain names when
/// the viewer is not built in.
async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Response {
    match ASSETS.iter().find(|asset| asset.name == name) {
        Some(asset) => serve(asset, &headers),
        None if !embedded() && (name == "kiewer_viewer.js" || name == "kiewer_viewer_bg.wasm") => (
            StatusCode::SERVICE_UNAVAILABLE,
            [Policy::Personal.header()],
            NOT_BUILT,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [Policy::NotFound.header()],
            "no such asset\n",
        )
            .into_response(),
    }
}

/// `asset` in the coding `headers` take, with the headers an immutable,
/// negotiated response carries.
fn serve(asset: &Asset, headers: &HeaderMap) -> Response {
    let encoding = Encoding::negotiate(headers);
    let common = [
        (CONTENT_TYPE, asset.content_type),
        (VARY, "Accept-Encoding"),
        Policy::Immutable.header(),
    ];
    match encoding.content_encoding() {
        Some(coding) => {
            (common, [(CONTENT_ENCODING, coding)], asset.bytes(encoding)).into_response()
        }
        None => (common, asset.bytes(encoding)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `url` is `prefix`, 16 lowercase hex characters and
    /// `suffix`: the shape `infra/fastly/recv.vcl.tftpl` matches to send
    /// brotli through, so a change to the hash's length shows here.
    fn hashed(url: &str, prefix: &str, suffix: &str) -> bool {
        url.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(|hash| {
                hash.len() == 16
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
    }

    #[test]
    fn the_script_and_wasm_urls_are_hashed_when_embedded_and_plain_when_not() {
        let script = script_url();
        let wasm = wasm_url();
        if embedded() {
            assert!(hashed(&script, "/viewer/kiewer_viewer.", ".js"), "{script}");
            assert!(
                hashed(&wasm, "/viewer/kiewer_viewer_bg.", ".wasm"),
                "{wasm}"
            );
            let asset = asset_of(SCRIPT_TYPE).expect("the script");
            let text = std::str::from_utf8(asset.bytes(Encoding::Identity)).expect("UTF-8");
            assert!(
                text.contains(asset_of(WASM_TYPE).expect("the wasm").name),
                "the script fetches the wasm by its hashed name"
            );
        } else {
            assert_eq!(script, "/viewer/kiewer_viewer.js");
            assert_eq!(wasm, "/viewer/kiewer_viewer_bg.wasm");
        }
    }
}
