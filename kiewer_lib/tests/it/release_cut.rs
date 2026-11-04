//! `scripts/release-cut.sh`: the release job cuts a Version only when it
//! sorts above every Release in git, a candidate before its Release, so a
//! green run of an older commit never moves production backwards (#97).
//!
//! Git is real: a bare remote carrying the tags, and a checkout with no
//! tags of its own whose `origin` is that remote. The script reads the
//! remote's tags only, so this stands in for CI's shallow, tagless
//! checkout without a commit to clone. The tags point at the empty tree,
//! since the script reads only their names. The job's outputs go to the
//! file `GITHUB_OUTPUT` names, as on a runner.
//!
//! No test execs a file this process wrote: the script's copy in the
//! checkout runs through `bash`, for the reason `prune_images` gives
//! (#108).
//!
//! A script's test, in `kiewer_lib`'s binary because the workspace has no
//! crate for its scripts and this one needs only cargo's scratch directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use kiewer_test_support::{TempDir, scratch_dir};
use test_case::test_case;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/release-cut.sh");
const CHECKOUT_SCRIPT: &str = "checkout/scripts/release-cut.sh";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A remote carrying `git_tags` and a checkout whose `origin` it is.
    fn new(git_tags: &[&str]) -> Self {
        let fixture = Self {
            dir: scratch_dir!(),
        };
        let root = fixture.dir.path();
        fixture.git(root, &["init", "--quiet", "--bare", "origin.git"]);
        let origin = root.join("origin.git");
        let tree = fixture.git(&origin, &["mktree"]);
        for tag in git_tags {
            fixture.git(&origin, &["update-ref", &format!("refs/tags/{tag}"), &tree]);
        }
        fixture.git(root, &["init", "--quiet", "checkout"]);
        let url = format!("file://{}", origin.to_str().expect("a utf-8 scratch path"));
        fixture.git(&root.join("checkout"), &["remote", "add", "origin", &url]);
        fs::create_dir(root.join("checkout/scripts")).expect("the scripts directory");
        fs::copy(SCRIPT, root.join(CHECKOUT_SCRIPT)).expect("the script in the checkout");
        fixture
    }

    /// The hook's environment holds `GIT_DIR` and `GIT_INDEX_FILE` for the
    /// commit being checked, which must reach neither this git nor the
    /// script's. `TMPDIR` keeps the script's throwaway repository in the
    /// scratch directory.
    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH is set"))
            .env("HOME", self.dir.path())
            .env("TMPDIR", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    /// Its stdout, trimmed: what `mktree` names.
    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self
            .command(Path::new("git"))
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8 from git")
            .trim()
            .to_owned()
    }

    /// Started from outside the checkout, as a run's `attempt`, with the
    /// Version `version`.
    fn decide(&self, version: &str, attempt: u32) -> Output {
        self.command(Path::new("bash"))
            .arg(self.dir.path().join(CHECKOUT_SCRIPT))
            .arg(version)
            .env("GITHUB_OUTPUT", self.file("outputs"))
            .env("GITHUB_RUN_ATTEMPT", attempt.to_string())
            .current_dir(self.dir.path())
            .output()
            .expect("the script runs")
    }

    /// The outputs of an attempt that succeeded.
    fn outputs_of(&self, version: &str, attempt: u32) -> String {
        let output = self.decide(version, attempt);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.outputs()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn outputs(&self) -> String {
        fs::read_to_string(self.file("outputs")).unwrap_or_default()
    }
}

fn kind_of(version: &str) -> &'static str {
    if version.contains("-rc.") {
        "candidate"
    } else {
        "release"
    }
}

fn outputs(version: &str, kind: &str, cut: bool, prune: bool) -> String {
    format!("version={version}\nkind={kind}\ncut={cut}\nprune={prune}\n")
}

#[test_case(&[], "0.1.0"; "a first Release")]
#[test_case(&[], "0.1.0-rc.1"; "a first candidate")]
#[test_case(&["v0.1.0", "v0.2.0"], "0.3.0"; "a newer Version")]
#[test_case(&["v0.9.0"], "0.10.0"; "a number of two digits above one of one digit")]
#[test_case(&["v0.99.0"], "1.0.0"; "a major above every minor")]
#[test_case(&["v0.2.0-rc.1"], "0.2.0"; "a Release after its candidate")]
#[test_case(&["v0.2.0-rc.1"], "0.2.0-rc.2"; "a second candidate")]
#[test_case(&["v0.2.0-rc.9"], "0.2.0-rc.10"; "a candidate number of two digits")]
#[test_case(&["v0.2.0"], "0.3.0-rc.1"; "a candidate of the next Version")]
#[test_case(&["v0.2.0", "vnext", "v9"], "0.3.0"; "a tag of no Release shape takes no place")]
fn a_version_above_every_release_is_cut(git_tags: &[&str], version: &str) {
    let fixture = Fixture::new(git_tags);

    assert_eq!(
        fixture.outputs_of(version, 1),
        outputs(version, kind_of(version), true, true)
    );
}

#[test_case(&["v0.2.0"], "0.2.0"; "the Version is tagged")]
#[test_case(&["v0.2.0-rc.1"], "0.2.0-rc.1"; "the candidate is tagged")]
#[test_case(&["v0.3.0"], "0.2.0"; "an older Version, its Bump finishing after a newer one")]
#[test_case(&["v0.2.0-rc.1", "v0.2.1"], "0.2.0"; "a late rerun of a red older Bump")]
#[test_case(&["v0.2.0"], "0.2.0-rc.1"; "a candidate after its Release")]
#[test_case(&["v0.2.0-rc.2"], "0.2.0-rc.1"; "an older candidate")]
#[test_case(&["v0.10.0"], "0.9.0"; "a number of one digit below one of two digits")]
#[test_case(&["v0.3.0-rc.1"], "0.2.0"; "a Release below a newer candidate")]
fn a_version_not_above_every_release_is_a_plain_push(git_tags: &[&str], version: &str) {
    let fixture = Fixture::new(git_tags);

    assert_eq!(
        fixture.outputs_of(version, 1),
        outputs(version, kind_of(version), false, false)
    );
}

/// A failed prune comes after the tag, so the rerun that repeats it finds
/// the Version tagged.
#[test_case(&["v0.2.0"], "0.2.0", false; "after a failed prune")]
#[test_case(&["v0.1.0"], "0.2.0", true; "after a failed deploy")]
fn a_rerun_prunes_whether_or_not_it_cuts(git_tags: &[&str], version: &str, cut: bool) {
    let fixture = Fixture::new(git_tags);

    assert_eq!(
        fixture.outputs_of(version, 2),
        outputs(version, "release", cut, true)
    );
}

#[test_case("0.2"; "two numbers")]
#[test_case("0.2.0-beta.1"; "another suffix")]
#[test_case("v0.2.0"; "a tag's spelling")]
#[test_case(""; "nothing")]
fn a_version_of_neither_shape_fails_with_no_outputs(version: &str) {
    let fixture = Fixture::new(&["v0.1.0"]);

    let output = fixture.decide(version, 1);

    assert!(!output.status.success());
    assert_eq!(fixture.outputs(), "");
}

/// An empty answer from a remote it cannot read would look like a first
/// Release.
#[test]
fn an_unreadable_remote_fails_with_no_outputs() {
    let fixture = Fixture::new(&["v0.1.0"]);
    fs::remove_dir_all(fixture.file("origin.git")).expect("the remote gone");

    let output = fixture.decide("0.2.0", 1);

    assert!(!output.status.success());
    assert_eq!(fixture.outputs(), "");
}
