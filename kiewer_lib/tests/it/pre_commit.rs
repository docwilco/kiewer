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

    /// The hook's environment holds `GIT_DIR` and `GIT_INDEX_FILE` for the
    /// commit being checked, which must reach neither this git nor the
    /// hook it runs.
    fn command(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(self.repo())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KIEWER_TEST_CHECKS_RAN", self.checks_ran())
            .env("BASH_FUNC_cargo%%", "() { [ \"$1\" != metadata ]; }")
            .output()
            .expect("git runs")
    }

    fn git(&self, args: &[&str]) {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// What the stand-in checks script was asked to run, one line a run.
    fn runs(&self) -> String {
        fs::read_to_string(self.checks_ran()).unwrap_or_default()
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.repo().join(Path::new(path)), contents).expect("a file in the repository");
    }
}

#[test]
fn an_amend_with_nothing_staged_runs_every_check() {
    let fixture = Fixture::new();
    fixture.git(&["commit", "--quiet", "--amend", "--no-edit"]);
    assert_eq!(fixture.runs(), "all\n");
}

#[test]
fn a_commit_of_prose_alone_skips_the_checks() {
    let fixture = Fixture::new();
    fixture.write("README.md", "# prose\n");
    fixture.git(&["add", "README.md"]);
    let output = fixture.command(&["commit", "--message", "prose"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.runs(), "");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("prose only"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_commit_of_code_runs_every_check() {
    let fixture = Fixture::new();
    fixture.write("main.rs", "fn main() { println!(); }\n");
    fixture.git(&["commit", "--quiet", "--all", "--message", "code"]);
    assert_eq!(fixture.runs(), "all\n");
}
