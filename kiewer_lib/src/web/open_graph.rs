//! Open Graph tags (docs/v1-scope.md, Pages and URLs): what Discord reads
//! from a page's `<head>` to unfurl a link. The design, branch, revision,
//! schematic and history pages carry one [`OpenGraph`] each, written by the
//! `open_graph.html` partial: title, description, the page's absolute
//! URL, and the revision's large thumbnail (ADR 0004) with its size once
//! it is rendered; until then the tags carry no image, since Discord
//! keeps whatever image it fetched first.

use super::AppState;
use crate::description::Description;
use crate::design::DesignHead;
use crate::error::Result;
use crate::revision::Metadata;
use crate::thumbnail::{self, Size};

/// The `og:image`: its absolute URL and its size in pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// One page's tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenGraph {
    pub title: String,
    pub description: String,
    /// The page's absolute URL.
    pub url: String,
    pub image: Option<Image>,
}

impl OpenGraph {
    /// The tags of the page at `path` under `public_url` (no trailing
    /// slash), with no image.
    #[must_use]
    pub fn new(public_url: &str, path: &str, title: String, description: String) -> Self {
        Self {
            title,
            description,
            url: format!("{public_url}{path}"),
            image: None,
        }
    }

    /// The same with the large thumbnail at `path` as the image.
    #[must_use]
    pub fn with_thumbnail(mut self, public_url: &str, path: &str) -> Self {
        let (width, height) = Size::Large.pixels();
        self.image = Some(Image {
            url: format!("{public_url}{path}"),
            width,
            height,
        });
        self
    }
}

/// The metadata as one line: its pairs joined, `Layers 26 (4 copper),
/// Nets 75, ...`.
#[must_use]
pub fn metadata_line(metadata: &Metadata) -> String {
    metadata
        .details()
        .into_iter()
        .map(|(label, value)| format!("{label} {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How much of a description the tag carries.
const DESCRIPTION_CHARS: usize = 200;

/// What the tag says about a design: the first line of its description
/// when it has one, else the revision's metadata, else who made it.
#[must_use]
pub fn design_description(shown: &DesignHead, description: &Description) -> String {
    let first_line = description
        .markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty());
    match first_line {
        Some(line) => line.chars().take(DESCRIPTION_CHARS).collect(),
        None => shown.revision.metadata.as_ref().map_or_else(
            || format!("A KiCad design by {}", shown.owner),
            metadata_line,
        ),
    }
}

/// The tags of a design page at `path` showing `shown`: the design's
/// name, [`design_description`], and the revision's large thumbnail once
/// it is rendered.
///
/// # Errors
///
/// [`thumbnail::of`]'s.
pub async fn for_design(
    state: &AppState,
    shown: &DesignHead,
    description: &Description,
    path: &str,
) -> Result<OpenGraph> {
    let public_url = state.auth.public_url();
    let og = OpenGraph::new(
        public_url,
        path,
        shown.design.name.clone(),
        design_description(shown, description),
    );
    Ok(match thumbnail::of(&state.db, shown.revision.hash).await? {
        Some(_) => og.with_thumbnail(
            public_url,
            &thumbnail::url(
                shown.owner.as_str(),
                shown.design.slug.as_str(),
                shown.revision.hash,
                Size::Large,
            ),
        ),
        None => og,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_is_absolute_and_the_image_is_the_large_thumbnail() {
        let og = OpenGraph::new(
            "https://kiewer.example",
            "/alice/demo",
            "demo".to_owned(),
            "a board".to_owned(),
        );
        assert_eq!(og.url, "https://kiewer.example/alice/demo");
        assert_eq!(og.image, None);
        let with = og.with_thumbnail("https://kiewer.example", "/alice/demo/large.png");
        assert_eq!(
            with.image,
            Some(Image {
                url: "https://kiewer.example/alice/demo/large.png".to_owned(),
                width: 1200,
                height: 630,
            })
        );
    }
}
