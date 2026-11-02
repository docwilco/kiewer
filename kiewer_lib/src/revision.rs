//! Trees and revisions as values: what is hashed and how, the paths a
//! tree admits, and the metadata a parsed board yields (ADR 0002;
//! GLOSSARY, Files and storage). Every hash here is computed by the
//! server from what it holds; the rows that persist them and the reads
//! over them are `design`.
//!
//! A tree hashes as the text `tree\n` followed by one `<blob hex>\t<path>\n`
//! line per entry, in byte order of the paths. A revision hashes as
//! git hashes a commit: `tree <hex>\n`, `parent <hex>\n` when it has one,
//! `author <handle>\n`, `time <iso 8601 utc>\n`, a blank line, and the
//! message when there is one. Both forms are fixed: a hash is a name
//! other servers and tests recompute.

use std::fmt;
use std::str::FromStr;

use kicad_core::geometry::{Arc, Bounds};
use kicad_core::model::{Board, Point, Shape};
use sha2::{Digest, Sha256};

use crate::account::Handle;
use crate::blob::BlobHash;
use crate::error::{Error, Result};
use crate::hex::sha256_name;

sha256_name!(
    TreeHash,
    "tree hash",
    "The hash of a tree: the SHA-256 of its canonical text ([`tree_hash`])."
);
sha256_name!(
    RevisionHash,
    "revision hash",
    "The hash of a revision: the SHA-256 of its canonical text ([`revision_hash`])."
);

/// A path in a tree: relative to the project folder, forward slashes,
/// no empty, `.` or `..` segment, no control character, no backslash,
/// at most 1024 bytes. What a browser's file name or a directory upload's
/// relative path gives, checked, since it goes in URLs, headers and the
/// tree's hashed text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreePath(String);

impl TreePath {
    /// The longest path admitted.
    pub const MAX_LEN: usize = 1024;

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last segment: the file's own name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// How many folders down the file sits: 0 in the project folder.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.0.matches('/').count()
    }

    /// The extension after the last dot of the file name, lowercased, or
    /// `None` without one.
    #[must_use]
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        let (_, extension) = name.rsplit_once('.')?;
        if extension.is_empty() {
            None
        } else {
            Some(extension.to_ascii_lowercase())
        }
    }

    /// Why `text` is not a path, or `None` when it is one.
    fn fault(text: &str) -> Option<&'static str> {
        if text.is_empty() {
            return Some("is empty");
        }
        if text.len() > Self::MAX_LEN {
            return Some("is longer than 1024 bytes");
        }
        if text.chars().any(char::is_control) {
            return Some("has a control character");
        }
        if text.contains('\\') {
            return Some("has a backslash; paths use forward slashes");
        }
        if text.starts_with('/') {
            return Some("is absolute");
        }
        if text.split('/').any(str::is_empty) {
            return Some("has an empty segment");
        }
        if text
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Some("has a . or .. segment");
        }
        None
    }
}

impl fmt::Display for TreePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TreePath {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        match Self::fault(text) {
            None => Ok(Self(text.to_owned())),
            Some(fault) => Err(Error::Malformed(format!("path {text:?} {fault}"))),
        }
    }
}

/// One file of a tree: where it sits and which blob it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: TreePath,
    pub blob: BlobHash,
    /// Uncompressed bytes, what the caps count; not part of the hash.
    pub size: u64,
}

/// The hash of these entries as one tree. The entries are taken in byte
/// order of their paths whatever order they come in, so two uploads of
/// the same files are one tree.
///
/// # Errors
///
/// [`Error::Malformed`] when two entries share a path: a tree is a map.
pub fn tree_hash(entries: &[TreeEntry]) -> Result<TreeHash> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = Sha256::new();
    hasher.update(b"tree\n");
    for pair in sorted.windows(2) {
        if pair[0].path == pair[1].path {
            return Err(Error::Malformed(format!(
                "path {:?} appears twice in the tree",
                pair[0].path
            )));
        }
    }
    for entry in sorted {
        hasher.update(format!("{}\t{}\n", entry.blob, entry.path).as_bytes());
    }
    Ok(TreeHash(hasher.finalize().into()))
}

/// What a revision's hash covers, before the hash exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionDraft<'a> {
    pub tree: TreeHash,
    pub parent: Option<RevisionHash>,
    /// The uploader's handle, the author text the hash covers.
    pub author: &'a Handle,
    /// The time as text, UTC, `2026-11-02T12:00:00.000000Z`: Postgres's
    /// `now()` formatted by the server, so the hash and the row agree.
    pub time: &'a str,
    pub message: Option<&'a str>,
}

/// The hash of this draft as a revision.
#[must_use]
pub fn revision_hash(draft: &RevisionDraft<'_>) -> RevisionHash {
    use std::fmt::Write as _;
    let mut text = format!("tree {}\n", draft.tree);
    // Writing to a String cannot fail.
    if let Some(parent) = draft.parent {
        let _ = writeln!(text, "parent {parent}");
    }
    let _ = writeln!(text, "author {}", draft.author);
    let _ = writeln!(text, "time {}", draft.time);
    text.push('\n');
    if let Some(message) = draft.message {
        text.push_str(message);
        text.push('\n');
    }
    RevisionHash(Sha256::digest(text.as_bytes()).into())
}

/// What the server reads off a board at upload and the design page shows
/// (docs/v1-scope.md, Uploads, S8).
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    /// The file format version, `20241229` and later.
    pub format_version: u32,
    /// Every layer in the board's layer table.
    pub layer_count: u32,
    /// The copper layers among them.
    pub copper_layer_count: u32,
    /// Width and height of the outline's bounding box, millimetres, or
    /// `None` for a board with nothing on `Edge.Cuts`.
    pub board_size: Option<(f64, f64)>,
    /// The font faces text items name, sorted and distinct, with
    /// `Newstroke` for the stroke font when any text names no face
    /// (GLOSSARY, Stroke font).
    pub fonts: Vec<String>,
    /// Nets with a name: KiCad's unnamed net 0 is not one.
    pub net_count: u32,
    pub footprint_count: u32,
}

/// The stroke font's name in [`Metadata::fonts`].
pub const STROKE_FONT: &str = "Newstroke";

/// The count as the row stores it. A board with more than two billion
/// items of one kind is not a board; the count saturates, it never panics.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Metadata {
    /// What `board` says about itself.
    #[must_use]
    pub fn of(board: &Board) -> Self {
        let mut fonts: Vec<String> = Vec::new();
        for text in board.all_texts() {
            let face = text.font.face.as_deref().unwrap_or(STROKE_FONT);
            if !fonts.iter().any(|known| known == face) {
                fonts.push(face.to_owned());
            }
        }
        fonts.sort();
        Self {
            format_version: board.version,
            layer_count: count(board.layers.len()),
            copper_layer_count: count(board.copper_layers().count()),
            board_size: outline_bounds(board).map(|bounds| (bounds.width(), bounds.height())),
            fonts,
            net_count: count(board.nets.iter().filter(|net| !net.name.is_empty()).count()),
            footprint_count: count(board.footprints.len()),
        }
    }

    /// The metadata as label and value pairs, in the order a design page
    /// lists them.
    #[must_use]
    pub fn details(&self) -> Vec<(&'static str, String)> {
        let mut details = vec![
            ("Format version", self.format_version.to_string()),
            (
                "Layers",
                format!("{} ({} copper)", self.layer_count, self.copper_layer_count),
            ),
        ];
        if let Some((width, height)) = self.board_size {
            details.push(("Board size", format!("{width:.2} x {height:.2} mm")));
        }
        details.push(("Fonts", self.fonts.join(", ")));
        details.push(("Nets", self.net_count.to_string()));
        details.push(("Footprints", self.footprint_count.to_string()));
        details
    }
}

/// The box around the board-level outline, or `None` without one. Exact
/// for lines, rectangles, polygons, arcs and circles; a Bezier curve
/// contributes its control points, which contain the curve.
fn outline_bounds(board: &Board) -> Option<Bounds> {
    board
        .outline()
        .filter_map(|graphic| shape_bounds(&graphic.drawing.shape))
        .reduce(Bounds::union)
}

fn shape_bounds(shape: &Shape) -> Option<Bounds> {
    match shape {
        Shape::Line { start, end } | Shape::Rect { start, end } => {
            Some(Bounds::of(*start).including(*end))
        }
        Shape::Arc { start, mid, end } => Some(match Arc::through(*start, *mid, *end) {
            Some(arc) => arc.bounds(),
            None => Bounds::of(*start).including(*mid).including(*end),
        }),
        Shape::Circle { center, end } => {
            let radius = (end.x - center.x).hypot(end.y - center.y);
            Some(Bounds {
                min: Point {
                    x: center.x - radius,
                    y: center.y - radius,
                },
                max: Point {
                    x: center.x + radius,
                    y: center.y + radius,
                },
            })
        }
        Shape::Polygon { points } | Shape::Curve { points } => {
            Bounds::around(points.iter().copied())
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    /// The hand-off board, the one real file in the repo.
    const BOARD: &str = include_str!("../../viewer-drop/board/midi8knob.kicad_pcb");

    fn entry(path: &str, bytes: &[u8]) -> TreeEntry {
        TreeEntry {
            path: path.parse().expect("a path"),
            blob: BlobHash::of(bytes),
            size: bytes.len() as u64,
        }
    }

    /// The literal is the shell's: `printf 'tree\n<sha of a>\tdemo.kicad_pcb\n<sha of b>\tdemo.kicad_sch\n' | sha256sum`.
    #[test]
    fn a_tree_hashes_its_sorted_entries_as_text() {
        let in_order = [entry("demo.kicad_pcb", b"a"), entry("demo.kicad_sch", b"b")];
        let reversed = [entry("demo.kicad_sch", b"b"), entry("demo.kicad_pcb", b"a")];
        let hash = tree_hash(&in_order).expect("a tree");
        assert_eq!(
            hash.to_string(),
            "6d9a9dd081e4a6d8ffef2a32b86b069eb8fa86d5268a3505ea0677f3b46f6956"
        );
        assert_eq!(
            tree_hash(&reversed).expect("a tree"),
            hash,
            "order does not matter"
        );
        assert_eq!(hash.short(), "6d9a9dd0");
        assert_eq!(
            "6d9a9dd081e4a6d8ffef2a32b86b069eb8fa86d5268a3505ea0677f3b46f6956"
                .parse::<TreeHash>()
                .expect("parses"),
            hash
        );
        assert_eq!(
            TreeHash::from_bytes(hash.as_bytes()).expect("32 bytes"),
            hash
        );
    }

    #[test]
    fn a_tree_refuses_a_path_twice() {
        let twice = [entry("x.kicad_pcb", b"a"), entry("x.kicad_pcb", b"b")];
        assert!(matches!(tree_hash(&twice), Err(Error::Malformed(_))));
    }

    /// The literals are the shell's over the canonical text with the tree
    /// hash above; see the module doc for the form.
    #[test]
    fn a_revision_hashes_tree_parent_author_time_and_message() {
        let tree: TreeHash = "6d9a9dd081e4a6d8ffef2a32b86b069eb8fa86d5268a3505ea0677f3b46f6956"
            .parse()
            .unwrap();
        let alice: Handle = "alice".parse().unwrap();
        let first = revision_hash(&RevisionDraft {
            tree,
            parent: None,
            author: &alice,
            time: "2026-11-02T12:00:00.000000Z",
            message: None,
        });
        assert_eq!(
            first.to_string(),
            "6da0f20b5d53135dbecca52d60fb77ba50a2cf637be3be968ed20712e8c9ea85"
        );
        let second = revision_hash(&RevisionDraft {
            tree,
            parent: Some(first),
            author: &alice,
            time: "2026-11-02T12:00:01.000000Z",
            message: Some("second"),
        });
        assert_eq!(
            second.to_string(),
            "db97dea607b238cf67d49cd9fd851d4d5f0f22374ce68ff8e2567ebbd60d3a7e"
        );
    }

    #[test_case("board.kicad_pcb", Some("kicad_pcb"); "a file name")]
    #[test_case("sub/dir/board.kicad_sch", Some("kicad_sch"); "a relative path")]
    #[test_case("Board.KICAD_PCB", Some("kicad_pcb"); "the extension is lowercased")]
    #[test_case("README", None; "no extension")]
    #[test_case("trailing.", None; "a trailing dot is no extension")]
    fn a_path_parses_and_names_its_extension(text: &str, extension: Option<&str>) {
        let path: TreePath = text.parse().expect("a path");
        assert_eq!(path.as_str(), text);
        assert_eq!(path.extension().as_deref(), extension);
    }

    #[test]
    fn a_path_knows_its_file_name() {
        let path: TreePath = "sub/dir/board.kicad_pcb".parse().unwrap();
        assert_eq!(path.file_name(), "board.kicad_pcb");
        assert_eq!(path.depth(), 2);
        let flat: TreePath = "board.kicad_pcb".parse().unwrap();
        assert_eq!(flat.file_name(), "board.kicad_pcb");
        assert_eq!(flat.depth(), 0);
    }

    #[test_case(""; "empty")]
    #[test_case("/etc/passwd"; "absolute")]
    #[test_case("a//b"; "an empty segment")]
    #[test_case("../x.kicad_pcb"; "a parent segment")]
    #[test_case("./x.kicad_pcb"; "a dot segment")]
    #[test_case("a\\b.kicad_pcb"; "a backslash")]
    #[test_case("a\tb.kicad_pcb"; "a tab, which the tree text uses")]
    #[test_case("a\nb.kicad_pcb"; "a newline, which the tree text uses")]
    #[test_case("a\0b"; "a nul")]
    fn a_path_outside_the_rules_is_refused_naming_the_rule(text: &str) {
        let err = text.parse::<TreePath>().expect_err("refused");
        assert!(matches!(err, Error::Malformed(_)), "{err}");
    }

    #[test]
    fn a_path_longer_than_the_limit_is_refused() {
        let long = "a".repeat(TreePath::MAX_LEN + 1);
        assert!(long.parse::<TreePath>().is_err());
        let just = "a".repeat(TreePath::MAX_LEN);
        assert!(just.parse::<TreePath>().is_ok());
    }

    /// The counts are the file's: `grep -c '^\t(footprint'` gives 78, the
    /// layer table has 26 entries of which 4 end in `.Cu`, 75 distinct
    /// `(net "...")` names appear, and the only face named is Consolas
    /// (89 times), beside texts with no face.
    #[test]
    fn the_hand_off_boards_metadata_matches_the_file() {
        let board = kicad_core::parse_board(BOARD).expect("the hand-off board parses");
        let metadata = Metadata::of(&board);
        assert_eq!(metadata.format_version, 20_260_206);
        assert_eq!(metadata.layer_count, 26);
        assert_eq!(metadata.copper_layer_count, 4);
        assert_eq!(metadata.net_count, 75);
        assert_eq!(metadata.footprint_count, 78);
        assert_eq!(metadata.fonts, ["Consolas", STROKE_FONT]);
        let (width, height) = metadata.board_size.expect("an outline");
        assert!(width > 10.0 && width < 500.0, "{width}");
        assert!(height > 10.0 && height < 500.0, "{height}");
        let details = metadata.details();
        assert_eq!(details[0], ("Format version", "20260206".to_owned()));
        assert_eq!(details[1], ("Layers", "26 (4 copper)".to_owned()));
        assert!(details[2].1.ends_with(" mm"), "{:?}", details[2]);
        assert_eq!(details[3], ("Fonts", "Consolas, Newstroke".to_owned()));
    }

    #[test]
    fn a_board_without_an_outline_has_no_size() {
        let board = kicad_core::parse_board(
            "(kicad_pcb (version 20241229) (generator \"test\") (layers (0 \"F.Cu\" signal) (44 \"Edge.Cuts\" user)))",
        )
        .expect("a bare board parses");
        let metadata = Metadata::of(&board);
        assert_eq!(metadata.board_size, None);
        assert!(metadata.fonts.is_empty());
        assert_eq!(metadata.layer_count, 2);
        assert_eq!(metadata.copper_layer_count, 1);
        assert!(
            !metadata
                .details()
                .iter()
                .any(|(label, _)| *label == "Board size")
        );
    }
}
