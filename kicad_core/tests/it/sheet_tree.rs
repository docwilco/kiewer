//! The sheet tree of a hierarchical schematic (docs/v1-scope.md,
//! Schematic): the fixture under `tests/fixtures/sheet_tree` is a root
//! with two sheet symbols, `power` beside it and `io` in a subfolder,
//! and `power` holds a third, `reg`, that reuses `io`'s file; the tree,
//! the sheet paths that name its nodes, and the fragment they travel in.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use kicad_core::parse_schematic;
use kicad_core::schematic::model::Sheet;
use kicad_core::schematic::sheet_tree::{SheetNode, SheetPath, build};
use kiewer_assert::assert_is_empty;
use test_case::test_case;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sheet_tree");

/// The fixture's sheets by their path in the revision's tree.
fn sheets() -> &'static BTreeMap<String, Sheet> {
    static SHEETS: OnceLock<BTreeMap<String, Sheet>> = OnceLock::new();
    SHEETS.get_or_init(|| {
        ["hier.kicad_sch", "power.kicad_sch", "sub/io.kicad_sch"]
            .into_iter()
            .map(|path| {
                let source = std::fs::read_to_string(format!("{FIXTURES}/{path}"))
                    .unwrap_or_else(|error| panic!("{path} is in the repo: {error}"));
                let sheet =
                    parse_schematic(&source).unwrap_or_else(|error| panic!("{path}: {error}"));
                (path.to_owned(), sheet)
            })
            .collect()
    })
}

fn tree() -> &'static SheetNode {
    static TREE: OnceLock<SheetNode> = OnceLock::new();
    TREE.get_or_init(|| build("hier.kicad_sch", sheets()).expect("the root is among the sheets"))
}

#[test]
fn the_parser_keeps_the_uuids_the_sheet_tree_is_named_by() {
    let root = sheets().get("hier.kicad_sch").expect("the root");
    assert_eq!(
        root.uuid.as_deref(),
        Some("11111111-0000-0000-0000-000000000001")
    );
    assert_eq!(root.sheet_symbols.len(), 2);
    assert_eq!(
        root.sheet_symbols[0].uuid.as_deref(),
        Some("77777777-0000-0000-0000-000000000001")
    );
    assert_eq!(root.sheet_symbols[0].name(), Some("power"));
    assert_eq!(root.sheet_symbols[1].file(), Some("sub/io.kicad_sch"));
}

#[test]
fn the_tree_hangs_every_sheet_symbol_under_its_sheet_with_its_file_resolved() {
    let root = tree();
    assert_eq!(root.name, "hier");
    assert_eq!(root.file.as_deref(), Some("hier.kicad_sch"));
    assert!(root.segment.is_empty(), "the root has no segment");
    assert_eq!(root.children.len(), 2);
    let power = &root.children[0];
    assert_eq!(
        (power.name.as_str(), power.segment.as_str()),
        ("power", "power")
    );
    assert_eq!(power.file.as_deref(), Some("power.kicad_sch"));
    assert_eq!(power.symbol, Some(0));
    let io = &root.children[1];
    assert_eq!(io.name, "io");
    assert_eq!(
        io.file.as_deref(),
        Some("sub/io.kicad_sch"),
        "a file in a subfolder is found beside its parent"
    );
    assert_is_empty!(io.children);
    // power's own sheet symbol reuses io's file: a second node, its own
    // path, the same file.
    assert_eq!(power.children.len(), 1);
    let reg = &power.children[0];
    assert_eq!(reg.name, "reg");
    assert_eq!(reg.file.as_deref(), Some("sub/io.kicad_sch"));
    assert_eq!(
        reg.uuid.as_deref(),
        Some("77777777-0000-0000-0000-000000000011")
    );
}

#[test]
fn the_walk_lists_every_node_in_reading_order_with_its_path() {
    let listed: Vec<(String, &str)> = tree()
        .walk()
        .into_iter()
        .map(|(path, node)| (path.to_string(), node.name.as_str()))
        .collect();
    assert_eq!(
        listed,
        [
            ("/", "hier"),
            ("/power", "power"),
            ("/power/reg", "reg"),
            ("/io", "io")
        ]
        .map(|(path, name)| (path.to_owned(), name))
    );
}

#[test_case("", Some("hier.kicad_sch"); "the root")]
#[test_case("power", Some("power.kicad_sch"); "a child by name")]
#[test_case("power/reg", Some("sub/io.kicad_sch"); "a grandchild")]
#[test_case("io", Some("sub/io.kicad_sch"); "the same file under another path")]
#[test_case("77777777-0000-0000-0000-000000000002", Some("sub/io.kicad_sch"); "a segment may be the sheet symbol's uuid")]
#[test_case("reg", None; "a grandchild is not a child")]
#[test_case("power/nope", None; "a name no sheet symbol has")]
fn a_sheet_path_resolves_to_a_node_or_nothing(path: &str, file: Option<&str>) {
    let path = SheetPath::parse(path);
    let node = tree().resolve(&path);
    assert_eq!(node.and_then(|node| node.file.as_deref()), file);
}

#[test_case(&[], ""; "the root travels as no sheet")]
#[test_case(&["power"], "sheet=power"; "one level")]
#[test_case(&["power", "reg"], "sheet=power/reg"; "two levels")]
#[test_case(&["a/b", "c&d=e"], "sheet=a%2Fb/c%26d%3De"; "a name's own slashes and separators are escaped")]
#[test_case(&["Ünïcode"], "sheet=%C3%9Cn%C3%AFcode"; "non-ascii is percent-encoded as utf-8")]
fn a_sheet_path_round_trips_through_the_fragment(segments: &[&str], fragment: &str) {
    let path = SheetPath::new(segments.iter().map(|s| (*s).to_owned()).collect());
    assert_eq!(path.fragment(), fragment);
    assert_eq!(SheetPath::from_fragment(fragment), path);
    assert_eq!(
        SheetPath::from_fragment(&format!("#{fragment}")),
        path,
        "with the hash"
    );
}

#[test]
fn the_fragment_reader_takes_the_sheet_key_among_others_and_is_forgiving() {
    assert_eq!(
        SheetPath::from_fragment("#view=5&sheet=power&x=1"),
        SheetPath::parse("power")
    );
    assert_eq!(SheetPath::from_fragment("#view=5"), SheetPath::root());
    assert_eq!(
        SheetPath::from_fragment("sheet=%ZZ/a"),
        SheetPath::new(vec!["%ZZ".to_owned(), "a".to_owned()]),
        "a bad escape is kept as written"
    );
    assert_eq!(
        SheetPath::from_fragment("sheet=//a//"),
        SheetPath::parse("a"),
        "empty segments are dropped"
    );
    assert!(SheetPath::root().is_root());
    assert_eq!(
        SheetPath::parse("power/reg").parent(),
        SheetPath::parse("power")
    );
    assert_eq!(
        SheetPath::parse("power").child("reg"),
        SheetPath::parse("power/reg")
    );
}

#[test]
fn a_sheet_whose_file_the_revision_lacks_is_a_leaf_without_a_file() {
    let mut partial = sheets().clone();
    partial.remove("sub/io.kicad_sch");
    let root = build("hier.kicad_sch", &partial).expect("the root is there");
    let io = &root.children[1];
    assert_eq!(io.name, "io");
    assert_eq!(io.file, None);
    assert_is_empty!(io.children);
    assert_eq!(build("missing.kicad_sch", &partial), None);
}

#[test]
fn a_sheet_that_includes_itself_does_not_recurse() {
    // A root whose only sheet symbol names its own file.
    let source = r#"(kicad_sch (version 20250114) (generator "eeschema") (uuid "r") (lib_symbols)
      (sheet (at 10 10) (size 10 10) (uuid "s")
        (property "Sheetname" "me" (at 10 9 0) (effects (font (size 1.27 1.27))))
        (property "Sheetfile" "loop.kicad_sch" (at 10 21 0) (effects (font (size 1.27 1.27))))))"#;
    let mut files = BTreeMap::new();
    files.insert(
        "loop.kicad_sch".to_owned(),
        parse_schematic(source).expect("parses"),
    );
    let root = build("loop.kicad_sch", &files).expect("the root");
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].file.as_deref(), Some("loop.kicad_sch"));
    assert!(root.children[0].children.is_empty(), "the loop is cut");
}

#[test]
fn siblings_sharing_a_name_are_told_apart_by_uuid_in_the_path() {
    let source = r#"(kicad_sch (version 20250114) (generator "eeschema") (uuid "r") (lib_symbols)
      (sheet (at 10 10) (size 10 10) (uuid "first")
        (property "Sheetname" "twin" (at 10 9 0) (effects (font (size 1.27 1.27))))
        (property "Sheetfile" "a.kicad_sch" (at 10 21 0) (effects (font (size 1.27 1.27)))))
      (sheet (at 30 10) (size 10 10) (uuid "second")
        (property "Sheetname" "twin" (at 30 9 0) (effects (font (size 1.27 1.27))))
        (property "Sheetfile" "b.kicad_sch" (at 30 21 0) (effects (font (size 1.27 1.27))))))"#;
    let empty = r#"(kicad_sch (version 20250114) (generator "eeschema") (lib_symbols))"#;
    let mut files = BTreeMap::new();
    files.insert(
        "root.kicad_sch".to_owned(),
        parse_schematic(source).expect("parses"),
    );
    files.insert(
        "a.kicad_sch".to_owned(),
        parse_schematic(empty).expect("parses"),
    );
    files.insert(
        "b.kicad_sch".to_owned(),
        parse_schematic(empty).expect("parses"),
    );
    let root = build("root.kicad_sch", &files).expect("the root");
    assert_eq!(root.children[0].segment, "twin");
    assert_eq!(root.children[1].segment, "second");
    assert_eq!(
        root.resolve(&SheetPath::parse("twin"))
            .and_then(|n| n.file.as_deref()),
        Some("a.kicad_sch")
    );
    assert_eq!(
        root.resolve(&SheetPath::parse("second"))
            .and_then(|n| n.file.as_deref()),
        Some("b.kicad_sch")
    );
}
