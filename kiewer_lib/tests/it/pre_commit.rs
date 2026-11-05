//! `.githooks/pre-commit`: which commits it checks. A commit of prose
//! alone skips the checks; every other commit runs them, an amend with
//! nothing staged included, which lists no staged path and still commits
//! the amended commit's whole tree (#79). A snapshot whose target cargo
//! puts anywhere but the snapshot's own is refused before any check.
//!
//! Git is real, in a scratch repository whose `core.hooksPath` is the
//! workspace's own `.githooks`. Its `scripts/checks.sh` is a stand-in that
//! records its argument in a file outside the repository, so a run builds
//! nothing. Cargo, which the hook's sweeps call, is a stand-in too: a bash
//! function exported into git's environment, whose `cargo metadata` fails,
//! so the hook finds no target directory to sweep and leaves the
//! workspace's alone, and whose `cargo sweep` records its arguments. So is
//! cargo-sweep, there on every host, so the hook always reaches its sweep;
//! and sccache, which fails to start, so a test starts no server. A stand-in
//! stat that reports every atime as the epoch plays a filesystem that
//! records no reads.

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
echo "${CHECKS_SKIP-unset}" > "$KIEWER_TEST_CHECKS_SKIP"
"#;

/// The stand-in cargo: `cargo metadata` fails, `cargo sweep` records its
/// arguments, everything else succeeds.
const CARGO: &str =
    r#"() { [ "$1" != sweep ] || echo "$*" >> "$KIEWER_TEST_SWEEPS"; [ "$1" != metadata ]; }"#;

/// A stand-in cargo whose `cargo metadata` names a target directory
/// elsewhere, as a config file's `build.target-dir` would.
const CARGO_TARGET_ELSEWHERE: &str =
    r#"() { [ "$1" != metadata ] || echo '{"target_directory":"/elsewhere"}'; }"#;

struct Fixture {
    dir: TempDir,
    cargo: &'static str,
    /// Whether the filesystem plays one that records no reads.
    no_reads: bool,
}

impl Fixture {
    /// A repository with a stand-in checks script and one code file,
    /// committed without the hook.
    fn new() -> Self {
        Self::with_cargo(CARGO)
    }

    /// The same, with `cargo` as the stand-in cargo.
    fn with_cargo(cargo: &'static str) -> Self {
        let fixture = Self {
            dir: scratch_dir!(),
            cargo,
            no_reads: false,
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

    fn sweeps_file(&self) -> PathBuf {
        self.dir.path().join("sweeps")
    }

    /// What the stand-in cargo was asked to sweep, one line a call.
    fn sweeps(&self) -> String {
        fs::read_to_string(self.sweeps_file()).unwrap_or_default()
    }

    /// Git with the hook's environment left behind: `GIT_DIR` and
    /// `GIT_INDEX_FILE` for the commit being checked must reach neither
    /// this git nor the hook it runs. Asserts that git succeeded.
    fn git(&self, args: &[&str]) -> Output {
        let output = self.git_unchecked(args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The same, whatever git's status.
    fn git_unchecked(&self, args: &[&str]) -> Output {
        let mut git = Command::new("git");
        git.args(args)
            .current_dir(self.repo())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KIEWER_TEST_CHECKS_RAN", self.checks_ran())
            .env(
                "KIEWER_TEST_CHECKS_SKIP",
                self.dir.path().join("checks-skip"),
            )
            .env("BASH_FUNC_cargo%%", self.cargo)
            .env("KIEWER_TEST_SWEEPS", self.sweeps_file())
            .env("BASH_FUNC_cargo-sweep%%", "() { :; }")
            .env("BASH_FUNC_sccache%%", "() { return 1; }");
        if self.no_reads {
            git.env("BASH_FUNC_stat%%", "() { echo 0; }");
        }
        git.output().expect("git runs")
    }

    /// What the stand-in checks script was asked to run, one line a run.
    /// The stages the hook told the checks to skip, as it exported them.
    fn skipped(&self) -> String {
        fs::read_to_string(self.dir.path().join("checks-skip"))
            .expect("the checks ran")
            .trim_end()
            .to_owned()
    }

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

#[test]
fn a_target_directory_elsewhere_is_refused() {
    let fixture = Fixture::with_cargo(CARGO_TARGET_ELSEWHERE);
    fixture.write("main.rs");
    fixture.git(&["add", "--all"]);
    let output = fixture.git_unchecked(&["commit", "--message", "change"]);
    assert!(!output.status.success(), "{}", hook_output(&output));
    assert!(
        hook_output(&output).contains("a config file sets build.target-dir"),
        "{}",
        hook_output(&output)
    );
    assert_eq!(fixture.runs(), "");
}

#[test]
fn a_checked_commit_stamps_its_target_and_sweeps_it() {
    let fixture = Fixture::new();
    fixture.write("main.rs");
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--message", "change"]);
    let sweeps = fixture.sweeps();
    let calls: Vec<&str> = sweeps
        .lines()
        .map(|line| line.split_whitespace().take(2).last().unwrap_or(""))
        .collect();
    assert_eq!(calls, ["--stamp", "--file"], "{sweeps}");
}

#[test]
fn a_filesystem_that_records_no_reads_is_never_swept() {
    let mut fixture = Fixture::new();
    fixture.no_reads = true;
    fixture.write("main.rs");
    fixture.git(&["add", "--all"]);
    let output = fixture.git(&["commit", "--message", "change"]);
    assert!(
        hook_output(&output).contains("records no reads"),
        "{}",
        hook_output(&output)
    );
    assert_eq!(fixture.sweeps(), "");
    assert_eq!(fixture.runs(), "all\n");
}

#[test_case(&["main.rs"], "sqlx wasm browser" ; "a file outside every corner")]
#[test_case(&["kiewer_viewer/src/lib.rs"], "sqlx" ; "the viewer")]
#[test_case(&["kicad_core/src/lib.rs"], "sqlx" ; "the board parser")]
#[test_case(&["kiewer_lib/src/db.rs"], "wasm" ; "the server")]
#[test_case(&["kiewer_lib/templates/page.html"], "sqlx wasm" ; "a template")]
#[test_case(&["kiewer_lib/tests/it/upload.rs"], "sqlx wasm browser" ; "a server test")]
#[test_case(&["kiewer_lib/tests/it/browser.rs"], "sqlx wasm" ; "the browser tests")]
#[test_case(&["migrations/0014_x.sql"], "wasm" ; "a migration")]
#[test_case(&["main.rs", "Cargo.lock"], "" ; "the lock")]
#[test_case(&["main.rs", ".githooks/pre-commit"], "" ; "the hook")]
fn a_commit_skips_the_stages_whose_corner_it_leaves_alone(paths: &[&str], skipped: &str) {
    let fixture = Fixture::new();
    for path in paths {
        fixture.write(path);
    }
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--message", "change"]);
    assert_eq!(fixture.skipped(), skipped);
}

#[test]
fn an_amend_with_nothing_staged_skips_no_stage() {
    let fixture = Fixture::new();
    fixture.git(&["commit", "--amend", "--no-edit"]);
    assert_eq!(fixture.skipped(), "");
}
