//! `.githooks/pre-commit`: which commits it checks. A commit of prose
//! alone skips the checks; every other commit runs them, an amend with
//! nothing staged included, which lists no staged path and still commits
//! the amended commit's whole tree (#79).
//!
//! Git is real, in a scratch repository whose `core.hooksPath` is the
//! workspace's own `.githooks`. Its `scripts/checks.sh` is a stand-in that
//! records its argument in a file outside the repository, so a run builds
//! nothing. Cargo, which the hook's sweeps call, is a stand-in too: a bash
//! function exported into git's environment, whose `cargo metadata` fails,
//! so the hook finds no target directory to sweep and leaves the
//! workspace's alone.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kiewer_test_support::{TempDir, scratch_dir};
use test_case::test_case;

const HOOKS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../.githooks");

/// Run by the hook as `bash scripts/checks.sh all`, never executed
/// directly, so no test runs an executable it has just written (ETXTBSY,
/// #108).
const CHECKS: &str = r#"#!/usr/bin/env bash
echo "==> fmt"
echo "$1" >> "$KIEWER_TEST_CHECKS_RAN"
"#;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A repository with a stand-in checks script and one code file,
    /// committed without the hook.
    fn new() -> Self {
        let fixture = Self {
            dir: scratch_dir!(),
        };
        let repo = fixture.repo();
        fs::create_dir_all(repo.join("scripts")).expect("the scripts directory");
        fs::write(repo.join("scripts/checks.sh"), CHECKS).expect("the checks stand-in");
        fs::write(repo.join("main.rs"), "fn main() {}\n").expect("a code file");
        fixture.git(&["init", "--quiet", "-b", "main"]);
        let hooks = fs::canonicalize(HOOKS).expect("the workspace's hooks");
        let hooks = hooks.to_str().expect("a utf-8 workspace path");
        fixture.git(&["config", "core.hooksPath", hooks]);
        fixture.git(&["config", "user.name", "test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "--quiet", "--no-verify", "--message", "seed"]);
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn checks_ran(&self) -> PathBuf {
        self.dir.path().join("checks-ran")
    }

    /// Git with the hook's environment left behind: `GIT_DIR` and
    /// `GIT_INDEX_FILE` for the commit being checked must reach neither
    /// this git nor the hook it runs. Asserts that git succeeded.
    fn git(&self, args: &[&str]) -> Output {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.repo())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KIEWER_TEST_CHECKS_RAN", self.checks_ran())
            .env("BASH_FUNC_cargo%%", "() { [ \"$1\" != metadata ]; }")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// What the stand-in checks script was asked to run, one line a run.
    fn runs(&self) -> String {
        fs::read_to_string(self.checks_ran()).unwrap_or_default()
    }

    fn write(&self, path: &str) {
        let path = self.repo().join(Path::new(path));
        fs::create_dir_all(path.parent().expect("a path in the repository"))
            .expect("its directory");
        fs::write(path, "changed\n").expect("a file in the repository");
    }
}

/// Git sends a hook's output to its own stderr.
fn hook_output(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn an_amend_with_nothing_staged_runs_every_check() {
    let fixture = Fixture::new();
    let output = fixture.git(&["commit", "--amend", "--no-edit"]);
    assert_eq!(fixture.runs(), "all\n");
    assert!(
        hook_output(&output).contains("==> fmt"),
        "{}",
        hook_output(&output)
    );
}

#[test_case(&["README.md"], false ; "markdown at the root")]
#[test_case(&["docs/notes.md"], false ; "under docs")]
#[test_case(&["viewer-drop/board.txt"], false ; "under viewer-drop")]
#[test_case(&["main.rs"], true ; "code")]
#[test_case(&["README.md", "main.rs"], true ; "prose with code")]
#[test_case(&["notes/README.md"], true ; "markdown below the root")]
fn a_commit_runs_every_check_unless_it_is_prose_alone(paths: &[&str], checked: bool) {
    let fixture = Fixture::new();
    for path in paths {
        fixture.write(path);
    }
    fixture.git(&["add", "--all"]);
    let output = fixture.git(&["commit", "--message", "change"]);
    if checked {
        assert_eq!(fixture.runs(), "all\n");
    } else {
        assert_eq!(fixture.runs(), "");
        assert!(
            hook_output(&output).contains("prose only"),
            "{}",
            hook_output(&output)
        );
    }
}
