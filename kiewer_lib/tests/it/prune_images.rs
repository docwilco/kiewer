//! `scripts/prune-images.sh`: after the prune the registry holds exactly
//! the newest Releases and the newest candidate git lists, and every other
//! tag untouched.
//!
//! Git is real: a bare remote carrying the tags and a shallow, tagless
//! clone of it, which is what CI's checkout gives the script. The registry
//! is a stand-in `gcloud` on PATH over a file of tags, answering what
//! Artifact Registry answers: a refused delete of a tag it does not hold.
//!
//! No test execs a file this process wrote. The stand-in is a tracked file
//! put on PATH where it lies, and the script's copy in the checkout runs
//! through `bash`: a file open for writing while any other test thread
//! forks (git, the browser, other scripts in this one binary) is refused
//! at exec with "Text file busy" (#108).
//!
//! A script's test, in `kiewer_lib`'s binary because the workspace has no
//! crate for its scripts and this one needs only cargo's scratch directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kiewer_test_support::{TempDir, scratch_dir};
use test_case::test_case;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/prune-images.sh");
const CHECKOUT_SCRIPT: &str = "checkout/scripts/prune-images.sh";

/// Holds the `gcloud` stand-in, which reads the fixture's files from
/// `PRUNE_FIXTURE_ROOT`.
const STAND_INS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/it/prune_images/bin");

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A remote carrying `git_tags`, a shallow tagless clone of it, and a
    /// registry holding `registry_tags`.
    fn new(git_tags: &[&str], registry_tags: &[&str]) -> Self {
        let dir = scratch_dir!();
        let root = dir.path().to_owned();
        let root = root.as_path();
        fs::write(root.join("registry"), lines(registry_tags)).expect("the registry");

        let fixture = Self { dir };
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        fixture.git(
            root,
            &["init", "--quiet", "--bare", "-b", "main", "origin.git"],
        );
        fixture.git(root, &["init", "--quiet", "-b", "main", "seed"]);
        fixture.git(
            &seed,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "--message",
                "seed",
            ],
        );
        for tag in git_tags {
            fixture.git(&seed, &["tag", tag]);
        }
        let origin = origin.to_str().expect("a utf-8 scratch path");
        fixture.git(&seed, &["push", "--quiet", "--tags", origin, "main"]);
        let url = format!("file://{origin}");
        fixture.git(
            root,
            &[
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                &url,
                "checkout",
            ],
        );
        fs::create_dir(root.join("checkout/scripts")).expect("the scripts directory");
        fs::copy(SCRIPT, root.join(CHECKOUT_SCRIPT)).expect("the script in the checkout");
        fixture
    }

    /// The hook's environment holds `GIT_DIR` and `GIT_INDEX_FILE` for the
    /// commit being checked, which must reach neither this git nor the
    /// script's.
    fn command(&self, program: &Path) -> Command {
        let mut path = vec![PathBuf::from(STAND_INS)];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH is set"),
        ));
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::join_paths(path).expect("a joinable PATH"))
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn git(&self, cwd: &Path, args: &[&str]) {
        let output = self
            .command(Path::new("git"))
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Started from outside the checkout: the script finds its repository
    /// from its own path, as the other scripts do.
    fn prune(&self) -> Output {
        self.command(Path::new("bash"))
            .arg(self.dir.path().join(CHECKOUT_SCRIPT))
            .env("GCP_PROJECT_ID", "a-project")
            .env("PRUNE_FIXTURE_ROOT", self.dir.path())
            .current_dir(self.dir.path())
            .output()
            .expect("the script runs")
    }

    fn assert_prune_succeeds(&self) {
        let output = self.prune();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn registry(&self) -> Vec<String> {
        let mut tags: Vec<String> = fs::read_to_string(self.file("registry"))
            .expect("the registry")
            .lines()
            .map(str::to_owned)
            .collect();
        tags.sort();
        tags
    }

    fn deleted(&self) -> String {
        fs::read_to_string(self.file("deleted")).unwrap_or_default()
    }
}

fn lines(tags: &[&str]) -> String {
    tags.iter().flat_map(|tag| [*tag, "\n"]).collect()
}

fn sorted(tags: &[&str]) -> Vec<String> {
    let mut tags: Vec<String> = tags.iter().map(ToString::to_string).collect();
    tags.sort();
    tags
}

#[test]
fn keeps_three_releases_one_candidate_and_every_other_tag() {
    let fixture = Fixture::new(
        &[
            "v0.1.0",
            "v0.2.0",
            "v0.3.0",
            "v0.4.0",
            "v0.4.0-rc.1",
            "v0.5.0-rc.1",
            "v0.5.0-rc.2",
        ],
        &[
            "0.1.0",
            "0.2.0",
            "0.3.0",
            "0.4.0",
            "0.4.0-rc.1",
            "0.5.0-rc.1",
            "0.5.0-rc.2",
            "latest",
            "1a2b3c4",
            "abcdef0",
        ],
    );

    fixture.assert_prune_succeeds();

    assert_eq!(
        fixture.registry(),
        sorted(&[
            "0.2.0",
            "0.3.0",
            "0.4.0",
            "0.5.0-rc.2",
            "latest",
            "1a2b3c4",
            "abcdef0"
        ]),
    );
}

#[test_case(
    &["v0.1.0", "v0.2.0", "v0.9.0", "v0.10.0"],
    &["0.2.0", "0.9.0", "0.10.0"];
    "a number of two digits is newer than one of one digit"
)]
#[test_case(
    &["v0.99.0", "v1.0.0", "v0.100.0", "v0.100.1", "v0.100.2"],
    &["0.100.2", "0.100.1", "1.0.0"];
    "the major outranks the minor and the minor the patch"
)]
#[test_case(
    &["v0.1.0-rc.2", "v0.1.0-rc.10", "v0.1.0-rc.9"],
    &["0.1.0-rc.10"];
    "the same holds for a candidate's number"
)]
#[test_case(
    &["v0.2.0-rc.1", "v0.10.0-rc.1"],
    &["0.10.0-rc.1"];
    "a candidate of a later Version is the newer candidate"
)]
#[test_case(
    &["v0.1.0", "v0.1.0-rc.1", "v0.2.0-rc.1"],
    &["0.1.0", "0.2.0-rc.1"];
    "a candidate takes no place among the Releases"
)]
#[test_case(
    &["v0.1.0-rc.1"],
    &["0.1.0-rc.1"];
    "a first Release may be a candidate"
)]
fn newest_is_by_version_order(git_tags: &[&str], kept: &[&str]) {
    let registry: Vec<&str> = git_tags
        .iter()
        .map(|tag| tag.trim_start_matches('v'))
        .collect();
    let fixture = Fixture::new(git_tags, &registry);

    fixture.assert_prune_succeeds();

    assert_eq!(fixture.registry(), sorted(kept));
}

#[test]
fn a_second_run_deletes_nothing() {
    let fixture = Fixture::new(
        &["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0"],
        &["0.1.0", "0.2.0", "0.3.0", "0.4.0"],
    );

    fixture.assert_prune_succeeds();
    assert_eq!(fixture.deleted(), "0.1.0\n");

    fixture.assert_prune_succeeds();
    assert_eq!(fixture.deleted(), "0.1.0\n");
}

/// A cut whose tag push was refused named its image and made no git tag,
/// so the Release-shaped registry tag has no line in git's list.
#[test]
fn a_release_shaped_tag_git_does_not_list_goes() {
    let fixture = Fixture::new(&["v0.1.0"], &["0.1.0", "0.2.0", "0.2.0-rc.1", "latest"]);

    fixture.assert_prune_succeeds();

    assert_eq!(fixture.registry(), sorted(&["0.1.0", "latest"]));
}

#[test]
fn a_kept_release_the_registry_lost_is_no_failure() {
    let fixture = Fixture::new(&["v0.1.0", "v0.2.0"], &["0.2.0"]);

    fixture.assert_prune_succeeds();

    assert_eq!(fixture.registry(), sorted(&["0.2.0"]));
}

/// Only the two shapes count: `v1` is no Release, so `v0.1.0` to `v0.3.0`
/// are the newest three.
#[test]
fn other_v_tags_take_no_place() {
    let fixture = Fixture::new(
        &[
            "v0.1.0",
            "v0.2.0",
            "v0.3.0",
            "v1",
            "v1.0",
            "vnext",
            "v2.0.0-beta.1",
        ],
        &["0.1.0", "0.2.0", "0.3.0"],
    );

    fixture.assert_prune_succeeds();

    assert_eq!(fixture.registry(), sorted(&["0.1.0", "0.2.0", "0.3.0"]));
}

/// The prune runs after a cut, so an empty list means the read went wrong,
/// and read as "keep nothing" it would empty the registry.
#[test_case(&[]; "no tags")]
#[test_case(&["v1", "vnext"]; "no tag of a Release's shape")]
fn git_listing_no_release_deletes_nothing_and_fails(git_tags: &[&str]) {
    let registry = ["0.1.0", "0.2.0-rc.1", "latest"];
    let fixture = Fixture::new(git_tags, &registry);

    let output = fixture.prune();

    assert!(!output.status.success());
    assert_eq!(fixture.registry(), sorted(&registry));
}

#[test]
fn a_refused_delete_fails_the_run() {
    let fixture = Fixture::new(
        &["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0"],
        &["0.1.0", "0.2.0", "0.3.0", "0.4.0"],
    );
    fs::write(fixture.file("refuse-deletes"), "").expect("the refusal");

    let output = fixture.prune();

    assert!(!output.status.success());
    assert_eq!(fixture.registry().len(), 4);
}
