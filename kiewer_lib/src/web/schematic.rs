//! The schematic page: a design's sheets in the viewer, one at a time,
//! with the sheet tree in a side panel (docs/v1-scope.md, Schematic).
//! At `/schematic` the sheet is the hand-off's at a fixed URL, as
//! `/viewer` shows the hand-off board; at `/{handle}/{slug}/schematic`
//! (and under `/b/{branch}` and `/r/{hash}`, as the design page has
//! them) the sheets are the revision's: the server parses every
//! `.kicad_sch` of the revision, builds the tree below the root sheet
//! (`kicad_core::schematic::sheet_tree`) and renders one row per node
//! with its sheet path, its fragment and its blob's URL. The page keeps
//! the sheet path in the URL fragment (`#sheet=power/reg`), fetches that
//! node's blob and calls `load_schematic`; a click on a sheet symbol in
//! the canvas is the same move to the child's fragment. The net block is
//! the board page's, over the same wasm methods.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, Response};
use axum::routing::get;
use kicad_core::schematic::model::Sheet;
use kicad_core::schematic::sheet_tree::{self, SheetNode, SheetPath};

use super::history::{self, Linked};
use super::open_graph::{self, OpenGraph};
use super::viewer::{NOT_BUILT, embedded, script_url};
use super::{AppState, blob};
use crate::account::Handle;
use crate::blob::blocking;
use crate::cache::{self, Key};
use crate::description;
use crate::design::{self, At, BranchName, DesignHead, Slug};
use crate::error::Result;
use crate::revision::RevisionHash;

/// The schematic routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/schematic", get(hand_off))
        .route(SCHEMATIC_PATH, get(schematic))
        .route("/{handle}/{slug}/schematic", get(page))
        .route("/{handle}/{slug}/b/{branch}/schematic", get(at_branch))
        .route("/{handle}/{slug}/r/{hash}/schematic", get(at_revision))
}

/// Where the hand-off page fetches its sheet from.
pub const SCHEMATIC_PATH: &str = "/schematic/midi8knob.kicad_sch";

/// The hand-off schematic, in the binary.
const SCHEMATIC: &str = include_str!("../../../viewer-drop/board/midi8knob.kicad_sch");

/// The URL of a design's schematic page, at the head of its default
/// branch.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug) -> String {
    format!("/{handle}/{slug}/schematic")
}

/// The URL of a design's schematic page at `at`: under the design page's
/// own URL for that branch or revision (`web/history.rs`'s forms, the
/// default branch as the bare page), so a redirect from the design page
/// keeps its context.
#[must_use]
pub fn url_at(handle: &Handle, slug: &Slug, default_branch: &str, at: At<'_>) -> String {
    let is_default = |branch: &BranchName| branch.as_str() == default_branch;
    match at {
        At::Default => url(handle, slug),
        At::Branch(branch) if is_default(branch) => url(handle, slug),
        At::Branch(branch) => format!("/{handle}/{slug}/b/{branch}/schematic"),
        At::Revision { hash, branch } => {
            let base = format!("/{handle}/{slug}/r/{hash}/schematic");
            match branch {
                Some(branch) if !is_default(branch) => format!("{base}?b={branch}"),
                _ => base,
            }
        }
    }
}

/// One row of the sheet panel: a node of the sheet tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetView {
    /// The row's position in the panel, the root first.
    pub index: usize,
    /// The row of the parent sheet; `None` for the root.
    pub parent: Option<usize>,
    /// The index of the sheet symbol in the parent sheet, which the
    /// viewer reports for a click; `None` for the root.
    pub symbol: Option<usize>,
    /// How many levels below the root.
    pub depth: usize,
    /// The sheet symbol's name, or the root file's stem.
    pub name: String,
    /// The sheet path as the breadcrumb shows it: `/power/reg`.
    pub path: String,
    /// The path as the fragment carries it: `sheet=power/reg`, empty for
    /// the root.
    pub fragment: String,
    /// Where the page fetches the sheet from; `None` when the revision
    /// lacks the file.
    pub url: Option<String>,
}

/// The rows for `tree`, the root first and each subtree before the next
/// sibling, with each node's file turned into a URL by `url_of`.
fn rows(tree: &SheetNode, url_of: &dyn Fn(&str) -> Option<String>) -> Vec<SheetView> {
    fn push(
        node: &SheetNode,
        path: &SheetPath,
        parent: Option<usize>,
        depth: usize,
        url_of: &dyn Fn(&str) -> Option<String>,
        out: &mut Vec<SheetView>,
    ) {
        let index = out.len();
        out.push(SheetView {
            index,
            parent,
            symbol: node.symbol,
            depth,
            name: node.name.clone(),
            path: path.to_string(),
            fragment: path.fragment(),
            url: node.file.as_deref().and_then(url_of),
        });
        for child in &node.children {
            push(
                child,
                &path.child(&child.segment),
                Some(index),
                depth + 1,
                url_of,
                out,
            );
        }
    }
    let mut out = Vec::new();
    push(tree, &SheetPath::root(), None, 0, url_of, &mut out);
    out
}

/// The schematic page.
#[derive(Template)]
#[template(path = "schematic.html")]
pub struct Page {
    pub build: String,
    pub embedded: bool,
    pub not_built: &'static str,
    /// Where the page imports the viewer's script from
    /// (`web/viewer.rs`).
    pub script_url: String,
    /// What the tab and the header name.
    pub title: String,
    /// The sheet panel, the root sheet first; empty for a revision
    /// without a schematic.
    pub sheets: Vec<SheetView>,
    /// The way back to the design, when there is one.
    pub back: Option<Back>,
    /// The Open Graph tags in the head (`web/open_graph.rs`): the page a
    /// design with no board opens on, so the page a link to it unfurls
    /// from. `None` for the hand-off sheet, which nobody links.
    pub og: Option<OpenGraph>,
}

/// The header's link back from the schematic page: the design's board
/// page, or the design page itself for a revision without a board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Back {
    pub url: String,
    pub label: &'static str,
}

impl Page {
    /// A page for `title` with the viewer's build state filled in.
    #[must_use]
    pub fn new(
        title: String,
        sheets: Vec<SheetView>,
        back: Option<Back>,
        og: Option<OpenGraph>,
    ) -> Self {
        Self {
            build: crate::build::line(),
            embedded: embedded(),
            not_built: NOT_BUILT.trim_end(),
            script_url: script_url(),
            title,
            sheets,
            back,
            og,
        }
    }
}

async fn hand_off() -> Result<Response> {
    let sheets = vec![SheetView {
        index: 0,
        parent: None,
        symbol: None,
        depth: 0,
        name: "midi8knob".to_owned(),
        path: SheetPath::root().to_string(),
        fragment: String::new(),
        url: Some(SCHEMATIC_PATH.to_owned()),
    }];
    let page = Page::new("midi8knob".to_owned(), sheets, None, None);
    Ok(cache::page(&[], Html(page.render()?)))
}

async fn schematic() -> Response {
    cache::page(
        &[],
        ([(CONTENT_TYPE, "text/plain; charset=utf-8")], SCHEMATIC),
    )
}

/// The sheet panel's rows for `shown`: every `.kicad_sch` of the
/// revision read from the store and parsed off the runtime's threads,
/// the tree built below the root sheet. A sheet that does not parse (the
/// upload refuses one, so only a damaged blob) is left out with a line
/// in the log, and its node is a leaf without a file.
async fn sheets_of(state: &AppState, shown: &DesignHead) -> Result<Vec<SheetView>> {
    let Some(root) = shown.schematic() else {
        return Ok(Vec::new());
    };
    let root_path = root.path.to_string();
    let mut texts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut urls: BTreeMap<String, String> = BTreeMap::new();
    for entry in shown
        .files
        .iter()
        .filter(|entry| entry.path.extension().as_deref() == Some("kicad_sch"))
    {
        if let Some(bytes) = state.blobs.get(&entry.blob).await? {
            texts.push((entry.path.to_string(), bytes));
            urls.insert(entry.path.to_string(), blob::url(&entry.blob));
        }
    }
    let tree = blocking(move || {
        let mut sheets: BTreeMap<String, Sheet> = BTreeMap::new();
        for (path, bytes) in texts {
            let parsed = String::from_utf8(bytes)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    kicad_core::parse_schematic(&text).map_err(|error| error.to_string())
                });
            match parsed {
                Ok(sheet) => {
                    sheets.insert(path, sheet);
                }
                Err(error) => tracing::warn!(%path, %error, "a stored sheet does not parse"),
            }
        }
        Ok(sheet_tree::build(&root_path, &sheets))
    })
    .await?;
    Ok(tree.map_or_else(Vec::new, |tree| {
        rows(&tree, &|file| urls.get(file).cloned())
    }))
}

/// The page for the design at `/{handle}/{slug}` shown at `at`, or 404
/// when there is no such design, branch or reachable revision, or the
/// revision holds no schematic. Two segments that are not a handle and a
/// slug are 404 before any lookup, as on the design page.
async fn show(state: &AppState, handle: &str, slug: &str, at: At<'_>) -> Result<Response> {
    let not_found = || cache::not_found_under(handle, "no such schematic\n");
    let (Ok(handle), Ok(slug)) = (handle.parse::<Handle>(), slug.parse::<Slug>()) else {
        return Ok(not_found());
    };
    let Some(shown) = design::show_at(&state.db, handle.as_str(), slug.as_str(), at).await? else {
        return Ok(not_found());
    };
    if shown.schematic().is_none() {
        return Ok(not_found());
    }
    let sheets = sheets_of(state, &shown).await?;
    let design_url = match at {
        At::Default | At::Branch(_) => {
            history::branch_url(&shown.owner, &shown.design, &shown.branch)
        }
        At::Revision { .. } => history::revision_url(
            &shown.owner,
            &shown.design,
            shown.revision.hash,
            &shown.branch,
        ),
    };
    // The way back: the board, or for a revision without one the design
    // page itself, asked to stay (it would otherwise send the browser
    // straight here again).
    let back = if shown.board().is_some() {
        Back {
            url: design_url,
            label: "board",
        }
    } else {
        let separator = if design_url.contains('?') { '&' } else { '?' };
        Back {
            url: format!("{design_url}{separator}design"),
            label: "design page",
        }
    };
    let description = description::of(&state.db, shown.design.id).await?;
    let path = url_at(
        &handle,
        &shown.design.slug,
        &shown.design.default_branch,
        at,
    );
    let og = open_graph::for_design(state, &shown, &description, &path).await?;
    let keys = [Key::Design(shown.design.id)];
    let page = Page::new(shown.design.name, sheets, Some(back), Some(og));
    Ok(cache::page(&keys, Html(page.render()?)))
}

/// `GET /{handle}/{slug}/schematic`: the head of the default branch.
async fn page(
    State(state): State<AppState>,
    Path((handle, slug)): Path<(String, String)>,
) -> Result<Response> {
    show(&state, &handle, &slug, At::Default).await
}

/// `GET /{handle}/{slug}/b/{branch}/schematic`: the branch's head; 404
/// for text that is not a branch name, before any lookup.
async fn at_branch(
    State(state): State<AppState>,
    Path((handle, slug, branch)): Path<(String, String, String)>,
) -> Result<Response> {
    let Ok(branch) = branch.parse::<BranchName>() else {
        return Ok(cache::not_found_under(&handle, "no such branch\n"));
    };
    show(&state, &handle, &slug, At::Branch(&branch)).await
}

/// `GET /{handle}/{slug}/r/{hash}/schematic`: one revision, when the
/// design reaches it; 404 otherwise, and for text that is not a hash.
async fn at_revision(
    State(state): State<AppState>,
    Path((handle, slug, hash)): Path<(String, String, String)>,
    Query(linked): Query<Linked>,
) -> Result<Response> {
    let Ok(hash) = hash.parse::<RevisionHash>() else {
        return Ok(cache::not_found_under(&handle, "no such revision\n"));
    };
    let branch = linked.b.and_then(|name| name.parse::<BranchName>().ok());
    let at = At::Revision {
        hash,
        branch: branch.as_ref(),
    };
    show(&state, &handle, &slug, at).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_list_the_tree_in_reading_order_with_parents_and_urls() {
        let source = r#"(kicad_sch (version 20250114) (generator "eeschema") (uuid "r") (lib_symbols)
          (sheet (at 10 10) (size 10 10) (uuid "a")
            (property "Sheetname" "power" (at 10 9 0) (effects (font (size 1.27 1.27))))
            (property "Sheetfile" "power.kicad_sch" (at 10 21 0) (effects (font (size 1.27 1.27)))))
          (sheet (at 30 10) (size 10 10) (uuid "b")
            (property "Sheetname" "io" (at 30 9 0) (effects (font (size 1.27 1.27))))
            (property "Sheetfile" "io.kicad_sch" (at 30 21 0) (effects (font (size 1.27 1.27))))))"#;
        let child = r#"(kicad_sch (version 20250114) (generator "eeschema") (uuid "c") (lib_symbols)
          (sheet (at 10 10) (size 10 10) (uuid "d")
            (property "Sheetname" "reg" (at 10 9 0) (effects (font (size 1.27 1.27))))
            (property "Sheetfile" "io.kicad_sch" (at 10 21 0) (effects (font (size 1.27 1.27))))))"#;
        let empty = r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols))"#;
        let mut sheets = BTreeMap::new();
        for (path, text) in [
            ("top.kicad_sch", source),
            ("power.kicad_sch", child),
            ("io.kicad_sch", empty),
        ] {
            sheets.insert(
                path.to_owned(),
                kicad_core::parse_schematic(text).expect("parses"),
            );
        }
        let tree = sheet_tree::build("top.kicad_sch", &sheets).expect("the root");
        let listed = rows(&tree, &|file| Some(format!("/blob/{file}")));
        // Each row as one line: index, parent, symbol, depth, name,
        // path, fragment.
        let summary: Vec<String> = listed
            .iter()
            .map(|row| {
                format!(
                    "{} {:?} {:?} {} {} {} {}",
                    row.index, row.parent, row.symbol, row.depth, row.name, row.path, row.fragment
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                "0 None None 0 top / ",
                "1 Some(0) Some(0) 1 power /power sheet=power",
                "2 Some(1) Some(0) 2 reg /power/reg sheet=power/reg",
                "3 Some(0) Some(1) 1 io /io sheet=io",
            ]
        );
        assert_eq!(listed[2].url.as_deref(), Some("/blob/io.kicad_sch"));
        assert_eq!(listed[3].url, listed[2].url, "one file, two rows");
    }

    #[test]
    fn the_page_url_follows_the_design_pages_branch_and_revision_forms() {
        let handle: Handle = "someone".parse().expect("a handle");
        let slug: Slug = "thing".parse().expect("a slug");
        let main: BranchName = "main".parse().expect("a branch");
        let dev: BranchName = "dev".parse().expect("a branch");
        let hash = RevisionHash::from_bytes(&[7u8; 32]).expect("a hash");
        let at = |at: At<'_>| url_at(&handle, &slug, "main", at);
        assert_eq!(at(At::Default), "/someone/thing/schematic");
        assert_eq!(
            at(At::Branch(&main)),
            "/someone/thing/schematic",
            "the default branch is the bare page"
        );
        assert_eq!(at(At::Branch(&dev)), "/someone/thing/b/dev/schematic");
        assert_eq!(
            at(At::Revision {
                hash,
                branch: Some(&dev)
            }),
            format!("/someone/thing/r/{hash}/schematic?b=dev")
        );
        assert_eq!(
            at(At::Revision {
                hash,
                branch: Some(&main)
            }),
            format!("/someone/thing/r/{hash}/schematic")
        );
        assert_eq!(
            at(At::Revision { hash, branch: None }),
            format!("/someone/thing/r/{hash}/schematic")
        );
    }
}
