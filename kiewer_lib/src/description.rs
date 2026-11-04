//! A design's description: Markdown the owner or an admin writes,
//! rendered here to the HTML the design page shows (docs/v1-scope.md,
//! Pages and URLs, Q48 and Q56). The rendering is comrak (`CommonMark` with
//! GitHub's tables and strikethrough) through an allow-list over the
//! syntax tree: what is not listed in `keep` is dropped before the
//! formatter sees it, so a new kind of node in a later comrak comes out
//! stripped, never leaked. Allowed: headings, paragraphs, emphasis,
//! strikethrough, lists, blockquotes, inline code and code blocks, tables,
//! and links to `http` and `https` URLs, written with
//! `rel="nofollow noopener"`. Dropped: raw HTML, images (their alt text
//! stays), every other link scheme (the link's text stays), thematic
//! breaks. The row keeps the Markdown and the HTML both: the page reads
//! the HTML as it is, and an edit renders again.

use std::fmt::Write as _;

use comrak::html::ChildRendering;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, Options, create_formatter, parse_document};
use sqlx::{PgConnection, PgPool};

use crate::design::DesignId;
use crate::error::Result;

/// The most Markdown a description holds, in bytes.
pub const MAX_LEN: usize = 64 * 1024;

/// The exact options: the two GitHub extensions the scope names, every
/// other extension and every parse and render option at its default. The
/// default `render.unsafe` is off, so even a node the allow-list let
/// through would have its raw HTML and `javascript:` URLs replaced.
fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options
}

/// What the allow-list does with one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The node and its children render.
    Node,
    /// The node goes; its children take its place (an image's alt text, a
    /// refused link's text).
    ChildrenOnly,
    /// The node goes with everything in it.
    Nothing,
}

/// Whether a link's destination is one the page follows: `http` or
/// `https`, any case, and nothing else (no `mailto`, no relative path, no
/// `javascript`).
fn web_url(url: &str) -> bool {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme);
    scheme.is_some_and(|scheme| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

fn keep(value: &NodeValue) -> Keep {
    match value {
        NodeValue::Document
        | NodeValue::BlockQuote
        | NodeValue::List(_)
        | NodeValue::Item(_)
        | NodeValue::CodeBlock(_)
        | NodeValue::Paragraph
        | NodeValue::Heading(_)
        | NodeValue::Table(_)
        | NodeValue::TableRow(_)
        | NodeValue::TableCell
        | NodeValue::Text(_)
        | NodeValue::SoftBreak
        | NodeValue::LineBreak
        | NodeValue::Code(_)
        | NodeValue::Emph
        | NodeValue::Strong
        | NodeValue::Strikethrough => Keep::Node,
        NodeValue::Link(link) if web_url(&link.url) => Keep::Node,
        NodeValue::Link(_) | NodeValue::Image(_) => Keep::ChildrenOnly,
        // Raw HTML (block and inline), thematic breaks, and every node a
        // disabled extension could not have made: the allow-list is what
        // it lists, so the rest is one arm.
        _ => Keep::Nothing,
    }
}

/// Apply the allow-list to the tree under `root`. The decisions are made
/// over the tree as parsed and applied after, so no node is moved while
/// the walk is on it.
fn prune<'a>(root: &'a AstNode<'a>) {
    let decided: Vec<(&'a AstNode<'a>, Keep)> = root
        .descendants()
        .map(|node| (node, keep(&node.data().value)))
        .filter(|(_, keep)| *keep != Keep::Node)
        .collect();
    for (node, keep) in decided {
        if keep == Keep::ChildrenOnly {
            let children: Vec<&AstNode<'a>> = node.children().collect();
            for child in children {
                child.detach();
                node.insert_before(child);
            }
        }
        node.detach();
    }
}

// comrak's own link renderer, plus the rel attribute. The URL is one
// `web_url` passed (prune ran first), and `escape_href` writes it.
create_formatter!(Formatter, {
    NodeValue::Link(ref link) => |context, entering| {
        if entering {
            context.write_str("<a href=\"")?;
            context.escape_href(&link.url)?;
            if !link.title.is_empty() {
                context.write_str("\" title=\"")?;
                context.escape(&link.title)?;
            }
            context.write_str("\" rel=\"nofollow noopener\">")?;
        } else {
            context.write_str("</a>")?;
        }
        return Ok(ChildRendering::HTML);
    },
});

/// `markdown` as the HTML the page shows.
#[must_use]
pub fn render(markdown: &str) -> String {
    let arena = Arena::new();
    let options = options();
    let root = parse_document(&arena, markdown, &options);
    prune(root);
    let mut html = String::new();
    // Writing to a String cannot fail.
    let _ = Formatter::format_document(root, &options, &mut html);
    html
}

/// A design's description as its row has it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Description {
    /// What the owner wrote: the form's text.
    pub markdown: String,
    /// What the page shows: [`render`] of the Markdown at the last edit.
    pub html: String,
}

/// The description of `design`.
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn of(db: &PgPool, design: DesignId) -> Result<Description> {
    // One read on the pool: the page's, beside the design's own read. Its
    // own statement rather than two more columns on every read of a design
    // row, since only the design page shows the description and only its
    // form writes it.
    let row = sqlx::query!(
        "SELECT description, description_html FROM design WHERE id = $1",
        design as DesignId
    )
    .fetch_one(db)
    .await?;
    Ok(Description {
        markdown: row.description,
        html: row.description_html,
    })
}

/// `markdown` becomes the description of `design`, rendered now, within
/// the transaction that saves the design's settings.
///
/// # Errors
///
/// [`Error::Db`](crate::Error::Db) when Postgres refuses.
pub async fn set(tx: &mut PgConnection, design: DesignId, markdown: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE design SET description = $2, description_html = $3 WHERE id = $1",
        design as DesignId,
        markdown,
        render(markdown)
    )
    .execute(tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("# Title\n\nA *para*graph with **bold**.", "<h1>Title</h1>\n<p>A <em>para</em>graph with <strong>bold</strong>.</p>\n"; "a heading and emphasis")]
    #[test_case("- one\n- two\n", "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n"; "a list")]
    #[test_case("> quoted\n", "<blockquote>\n<p>quoted</p>\n</blockquote>\n"; "a blockquote")]
    #[test_case("`x` and\n\n```\ncode\n```\n", "<p><code>x</code> and</p>\n<pre><code>code\n</code></pre>\n"; "inline code and a code block")]
    #[test_case("| a | b |\n|---|---|\n| c | d |\n", "<table>\n<thead>\n<tr>\n<th>a</th>\n<th>b</th>\n</tr>\n</thead>\n<tbody>\n<tr>\n<td>c</td>\n<td>d</td>\n</tr>\n</tbody>\n</table>\n"; "a table")]
    #[test_case("~~gone~~", "<p><del>gone</del></p>\n"; "strikethrough")]
    #[test_case("[site](https://example.com/a?b=c)", "<p><a href=\"https://example.com/a?b=c\" rel=\"nofollow noopener\">site</a></p>\n"; "an https link carries nofollow noopener")]
    #[test_case("[site](HTTP://example.com \"the title\")", "<p><a href=\"HTTP://example.com\" title=\"the title\" rel=\"nofollow noopener\">site</a></p>\n"; "an http link in any case with a title")]
    #[test_case("[run](javascript:alert(1))", "<p>run</p>\n"; "a javascript link is its text")]
    #[test_case("[mail](mailto:a@b.c) [here](/local) [data](data:text/html,x)", "<p>mail here data</p>\n"; "every other scheme is its text")]
    #[test_case("![alt text](https://example.com/x.png)", "<p>alt text</p>\n"; "an image is its alt text")]
    #[test_case("<div onclick=\"x()\">block</div>\n\ntext", "<p>text</p>\n"; "a raw HTML block is gone")]
    #[test_case("a <span>b</span> c <script>x</script>", "<p>a b c x</p>\n"; "inline tags are gone and the text between them stays")]
    #[test_case("<!-- a comment -->\n\n<img src=x onerror=alert(1)>\n", ""; "a comment and an img tag leave nothing")]
    #[test_case("above\n\n---\n\nbelow", "<p>above</p>\n<p>below</p>\n"; "a thematic break is gone")]
    #[test_case("1 < 2 & \"q\"", "<p>1 &lt; 2 &amp; &quot;q&quot;</p>\n"; "text is escaped")]
    #[test_case("", ""; "empty")]
    fn the_allow_list_decides_what_renders(markdown: &str, html: &str) {
        assert_eq!(render(markdown), html);
    }

    #[test]
    fn an_autolink_is_not_an_extension_here() {
        // No autolink extension: a bare URL is text, so the only links are
        // the ones written as links and checked above.
        assert_eq!(
            render("see https://example.com"),
            "<p>see https://example.com</p>\n"
        );
    }
}
