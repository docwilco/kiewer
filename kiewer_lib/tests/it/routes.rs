//! The rule on the server's fixed routes (docs/v1-scope.md, Operations):
//! none starts with `/_ah/` or ends in `z`, because Cloud Run's front end
//! answers those paths itself and the container never sees them (#121).
//! The test reads the source: axum's router cannot list its routes, and
//! three routes take their path from a constant.

use std::fs;
use std::path::Path;

use test_case::test_case;

/// What is wrong with `path` as a fixed route, if anything.
fn broken_by(path: &str) -> Option<&'static str> {
    let path = path.to_ascii_lowercase();
    if path.starts_with("/_ah/") {
        Some("Cloud Run reserves paths starting with /_ah/")
    } else if path.trim_end_matches('/').ends_with('z') {
        Some("Cloud Run's front end may answer paths ending in z itself")
    } else {
        None
    }
}

/// Every string literal in `source` that starts with `/`, with its line.
fn path_literals(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let mut rest = line;
        while let Some(open) = rest.find("\"/") {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else {
                break;
            };
            // An escaped quote ends the literal early, leaving its backslash.
            let literal = after[..close].trim_end_matches('\\');
            found.push((index + 1, literal.to_owned()));
            rest = &after[close + 1..];
        }
    }
    found
}

fn rust_files(dir: &Path, into: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

#[test_case("/_ah/health", true; "under /_ah/")]
#[test_case("/_ah/", true; "the /_ah/ root")]
#[test_case("/healthz", true; "ends in z")]
#[test_case("/{handle}/quiz", true; "ends in z under a parameter")]
#[test_case("/healthZ", true; "ends in uppercase Z")]
#[test_case("/quiz/", true; "ends in z and a slash")]
#[test_case("/_AH/x", true; "uppercase /_ah/")]
#[test_case("/health", false; "the health route")]
#[test_case("/_ahead", false; "starts like /_ah/ but is not")]
#[test_case("/zed/{id}", false; "a z inside")]
fn the_rule_catches_what_cloud_run_reserves(path: &str, broken: bool) {
    assert_eq!(broken_by(path).is_some(), broken, "{path}");
}

#[test]
fn literals_are_read_with_their_line() {
    let found = path_literals("a\nroute(\"/x\", y).route(\"/z\")\n");
    assert_eq!(found, [(2, "/x".to_owned()), (2, "/z".to_owned())]);
}

#[test]
fn a_literal_after_an_escaped_quote_loses_its_backslash() {
    let found = path_literals(r#"say(\"/quiz\") "#);
    assert_eq!(found, [(1, "/quiz".to_owned())]);
}

#[test]
fn no_fixed_route_is_reserved_by_cloud_run() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(!files.is_empty(), "no source under {}", root.display());
    let mut broken = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).expect("readable source");
        for (line, path) in path_literals(&source) {
            if let Some(reason) = broken_by(&path) {
                broken.push(format!("{}:{line}: {path:?}: {reason}", file.display()));
            }
        }
    }
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}
