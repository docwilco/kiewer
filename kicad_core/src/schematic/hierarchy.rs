//! The sheet tree of a schematic (docs/v1-scope.md, Schematic): the root
//! sheet's sheet symbols name child files, each a sheet with sheet
//! symbols of its own. A [`SheetPath`] names one node of that tree, root
//! first: each segment is the sheet symbol's name (its uuid when a
//! sibling shares the name), so the path reads in a URL. A file used
//! under two sheet symbols is two nodes with two paths and one file, as
//! KiCad instances it. [`build`] makes the tree from the sheets a
//! revision holds, keyed by their path in the revision's tree, resolving
//! each `Sheetfile` beside its parent, then at the root, then by name;
//! a file the revision lacks is a leaf with no file, and a sheet that
//! reaches itself is cut where it loops.
//!
//! The path travels in the page's URL fragment as `sheet=power/reg`, each
//! segment percent-encoded ([`SheetPath::fragment`]); the root is no
//! `sheet` key at all.

use std::collections::BTreeMap;
use std::fmt;

use super::model::Sheet;

/// How deep the tree is followed: a hierarchy deeper than this is a
/// loop the uuid check missed, not a design.
const MAX_DEPTH: usize = 32;

/// The segments naming a sheet from the root down; empty for the root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SheetPath(Vec<String>);

impl SheetPath {
    /// The root sheet.
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// A path of `segments`, root first; empty segments are dropped.
    #[must_use]
    pub fn new(segments: Vec<String>) -> Self {
        Self(
            segments
                .into_iter()
                .filter(|segment| !segment.is_empty())
                .collect(),
        )
    }

    /// The path `text` names, segments separated by `/`; empty ones are
    /// dropped, so `/power/` and `power` are the same.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self::new(text.split('/').map(str::to_owned).collect())
    }

    /// The segments, root first.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// Whether this is the root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The path one level up; the root's parent is the root.
    #[must_use]
    pub fn parent(&self) -> Self {
        let mut segments = self.0.clone();
        segments.pop();
        Self(segments)
    }

    /// The path of `segment` below this one.
    #[must_use]
    pub fn child(&self, segment: &str) -> Self {
        let mut segments = self.0.clone();
        segments.push(segment.to_owned());
        Self::new(segments)
    }

    /// The path as the URL fragment carries it, without the `#`:
    /// `sheet=power/reg`, each segment percent-encoded so a name's own
    /// slash, ampersand or equals sign survives; empty for the root.
    #[must_use]
    pub fn fragment(&self) -> String {
        if self.is_root() {
            return String::new();
        }
        let segments: Vec<String> = self.0.iter().map(|s| encode(s)).collect();
        format!("sheet={}", segments.join("/"))
    }

    /// The path a URL fragment carries, `#` or not, among any other keys
    /// (`view=5&sheet=power`): the root when it names no sheet. A bad
    /// percent escape is kept as written.
    #[must_use]
    pub fn from_fragment(fragment: &str) -> Self {
        let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
        fragment
            .split('&')
            .find_map(|pair| pair.strip_prefix("sheet="))
            .map_or_else(Self::root, |value| {
                Self::new(value.split('/').map(decode).collect())
            })
    }
}

impl fmt::Display for SheetPath {
    /// `/`, `/power`, `/power/reg`: the segments as written, for a
    /// breadcrumb.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            return f.write_str("/");
        }
        for segment in &self.0 {
            write!(f, "/{segment}")?;
        }
        Ok(())
    }
}

/// The upper-case hex digit of `nibble`, 0 to 15.
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('0', |digit| digit.to_ascii_uppercase())
}

/// `text` percent-encoded: RFC 3986's unreserved characters as they are,
/// every other byte of its UTF-8 as `%XX`.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

/// `text` percent-decoded as UTF-8; an escape that is not two hex digits,
/// or bytes that are not UTF-8, are kept as written.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = (byte == b'%')
            .then(|| {
                let high = hex(*bytes.get(index + 1)?)?;
                let low = hex(*bytes.get(index + 2)?)?;
                Some(high << 4 | low)
            })
            .flatten();
        if let Some(value) = escaped {
            out.push(value);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// One sheet in the tree: the root, or a sheet symbol of its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetNode {
    /// What the panel shows: the sheet symbol's name, or the root file's
    /// stem.
    pub name: String,
    /// The segment the node's path ends in: the name, or the uuid when a
    /// sibling shares the name; empty for the root.
    pub segment: String,
    /// The sheet symbol's uuid; `None` for the root or a symbol without.
    pub uuid: Option<String>,
    /// The index of the sheet symbol in the parent sheet's
    /// `sheet_symbols`; `None` for the root.
    pub symbol: Option<usize>,
    /// The sheet's file, as a path in the revision's tree; `None` when
    /// the revision has no such file.
    pub file: Option<String>,
    /// The sheets below, in the parent sheet's order.
    pub children: Vec<SheetNode>,
}

impl SheetNode {
    /// The node `path` names: each segment matched against a child's
    /// segment, then its name, then its uuid, the first child that
    /// matches winning. `None` when a segment matches nothing.
    #[must_use]
    pub fn resolve(&self, path: &SheetPath) -> Option<&SheetNode> {
        let mut node = self;
        for segment in path.segments() {
            node = node
                .children
                .iter()
                .find(|child| child.segment == *segment)
                .or_else(|| node.children.iter().find(|child| child.name == *segment))
                .or_else(|| {
                    node.children
                        .iter()
                        .find(|child| child.uuid.as_deref() == Some(segment.as_str()))
                })?;
        }
        Some(node)
    }

    /// Every node with its path, this one first, each subtree before the
    /// next sibling: the panel's order.
    #[must_use]
    pub fn walk(&self) -> Vec<(SheetPath, &SheetNode)> {
        let mut out = Vec::new();
        self.walk_into(&SheetPath::root(), &mut out);
        out
    }

    fn walk_into<'a>(&'a self, path: &SheetPath, out: &mut Vec<(SheetPath, &'a SheetNode)>) {
        out.push((path.clone(), self));
        for child in &self.children {
            child.walk_into(&path.child(&child.segment), out);
        }
    }
}

/// The tree below `root`, a path in `sheets`, which holds every parsed
/// `.kicad_sch` of the revision by its path. `None` when `sheets` lacks
/// the root.
#[must_use]
pub fn build(root: &str, sheets: &BTreeMap<String, Sheet>) -> Option<SheetNode> {
    let sheet = sheets.get(root)?;
    let name = root
        .rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.').map(|(stem, _)| stem))
        .unwrap_or(root)
        .to_owned();
    Some(SheetNode {
        name,
        segment: String::new(),
        uuid: None,
        symbol: None,
        file: Some(root.to_owned()),
        children: children(sheet, root, sheets, &mut vec![root.to_owned()]),
    })
}

/// The nodes for `sheet`'s sheet symbols; `ancestors` holds the files
/// from the root down to `sheet`'s, so a file among them is a loop.
fn children(
    sheet: &Sheet,
    file: &str,
    sheets: &BTreeMap<String, Sheet>,
    ancestors: &mut Vec<String>,
) -> Vec<SheetNode> {
    let mut nodes: Vec<SheetNode> = Vec::new();
    for (index, symbol) in sheet.sheet_symbols.iter().enumerate() {
        let name = symbol.name().unwrap_or("").to_owned();
        let taken = nodes.iter().any(|node| node.segment == name);
        let segment = if name.is_empty() || taken {
            symbol
                .uuid
                .clone()
                .unwrap_or_else(|| format!("{name}#{}", index + 1))
        } else {
            name.clone()
        };
        let resolved = symbol
            .file()
            .and_then(|sheetfile| resolve_file(file, sheetfile, sheets));
        let below = match &resolved {
            Some(path) if !ancestors.contains(path) && ancestors.len() < MAX_DEPTH => {
                match sheets.get(path) {
                    Some(child) => {
                        ancestors.push(path.clone());
                        let below = children(child, path, sheets, ancestors);
                        ancestors.pop();
                        below
                    }
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        nodes.push(SheetNode {
            name: if name.is_empty() {
                segment.clone()
            } else {
                name
            },
            segment,
            uuid: symbol.uuid.clone(),
            symbol: Some(index),
            file: resolved,
            children: below,
        });
    }
    nodes
}

/// The path in `sheets` that `sheetfile`, as a sheet symbol of `parent`
/// writes it, names: beside the parent (`..` and `.` folded), else from
/// the root, else the shallowest file of that name; `None` when none is
/// there.
fn resolve_file(parent: &str, sheetfile: &str, sheets: &BTreeMap<String, Sheet>) -> Option<String> {
    let sheetfile = sheetfile.replace('\\', "/");
    let parent_dir = parent.rsplit_once('/').map_or("", |(dir, _)| dir);
    let beside = normalise(&format!("{parent_dir}/{sheetfile}"));
    if sheets.contains_key(&beside) {
        return Some(beside);
    }
    let from_root = normalise(&sheetfile);
    if sheets.contains_key(&from_root) {
        return Some(from_root);
    }
    let wanted = sheetfile.rsplit('/').next().unwrap_or(&sheetfile);
    sheets
        .keys()
        .filter(|path| path.rsplit('/').next() == Some(wanted))
        .min_by_key(|path| (path.matches('/').count(), (*path).clone()))
        .cloned()
}

/// `path` with empty, `.` and `..` segments folded; a `..` past the root
/// is dropped.
fn normalise(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("a/b/../c.kicad_sch", "a/c.kicad_sch"; "a parent segment folds")]
    #[test_case("./x/./y", "x/y"; "dots and doubled slashes fold")]
    #[test_case("../../up", "up"; "past the root is dropped")]
    fn paths_normalise(path: &str, expected: &str) {
        assert_eq!(normalise(path), expected);
    }

    #[test]
    fn a_sheet_file_resolves_beside_its_parent_then_from_the_root_then_by_name() {
        let empty = r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols))"#;
        let mut sheets = BTreeMap::new();
        for path in [
            "top.kicad_sch",
            "sub/a.kicad_sch",
            "sub/deep/b.kicad_sch",
            "c.kicad_sch",
        ] {
            sheets.insert(
                path.to_owned(),
                crate::parse_schematic(empty).expect("parses"),
            );
        }
        assert_eq!(
            resolve_file("sub/a.kicad_sch", "deep/b.kicad_sch", &sheets).as_deref(),
            Some("sub/deep/b.kicad_sch")
        );
        assert_eq!(
            resolve_file("sub/a.kicad_sch", "c.kicad_sch", &sheets).as_deref(),
            Some("c.kicad_sch"),
            "not beside the parent, so from the root"
        );
        assert_eq!(
            resolve_file("top.kicad_sch", "elsewhere\\b.kicad_sch", &sheets).as_deref(),
            Some("sub/deep/b.kicad_sch"),
            "a backslashed path KiCad on Windows wrote still finds the file by name"
        );
        assert_eq!(
            resolve_file("top.kicad_sch", "none.kicad_sch", &sheets),
            None
        );
    }

    #[test]
    fn encoding_keeps_the_unreserved_and_escapes_the_rest() {
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
        assert_eq!(decode("a%20b%2Fc"), "a b/c");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("%4"), "%4");
        assert_eq!(decode("%FF"), "%FF", "not UTF-8, kept as written");
    }
}
