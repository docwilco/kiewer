//! `scripts/deploy-kiewer.sh`: a hand deploy of the production service
//! moves its traffic to the new revision, even after a Release candidate
//! pinned the traffic elsewhere (#93), and every run ends with where the
//! traffic went.
//!
//! Cloud Run is a stand-in `gcloud` on PATH that records each call, so the
//! tests read what the script asked for, in order.
//!
//! No test execs a file this process wrote: the stand-in is a tracked file
//! put on PATH where it lies, and the script runs through `bash`, for the
//! reason `prune_images` gives (#108).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use kiewer_test_support::{TempDir, scratch_dir};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/deploy-kiewer.sh");

/// The directory that holds the `gcloud` stand-in, which records into
/// `DEPLOY_FIXTURE_ROOT`.
const STAND_IN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/it/deploy_kiewer/bin");

/// The image the script names, with the `GCP_PROJECT_ID` the tests give it.
const IMAGE: &str = "europe-west4-docker.pkg.dev/a-project/kiewer/kiewer";

/// What the stand-in answers to a describe.
const DESCRIBED: &str = "https://kiewer-abc.a.run.app\tkiewer-00012\t\
    {'revisionName': 'kiewer-00012', 'percent': 100, 'latestRevision': True}\n";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: scratch_dir!(),
        }
    }

    fn deploy(&self, service: &str, version: &str) -> Output {
        let mut path = vec![PathBuf::from(STAND_IN_DIR)];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH is set"),
        ));
        Command::new("bash")
            .arg(SCRIPT)
            .args([service, version])
            .env_clear()
            .env("PATH", std::env::join_paths(path).expect("a joinable PATH"))
            .env("GCP_PROJECT_ID", "a-project")
            .env("DEPLOY_FIXTURE_ROOT", self.dir.path())
            .current_dir(self.dir.path())
            .output()
            .expect("the script runs")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.file("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn deploy_call(service: &str, tag: &str) -> String {
    format!(
        "run deploy {service} --project a-project --region europe-west4 \
         --image {IMAGE}:{tag} --quiet"
    )
}

fn describe_call(service: &str) -> String {
    format!(
        "run services describe {service} --project a-project --region europe-west4 \
         --format value(status.url, status.latestReadyRevisionName, status.traffic)"
    )
}

/// A candidate's `--no-traffic` keeps LATEST out of the traffic until
/// `--to-latest` puts it back, so a rollback or a candidate put in front
/// of users by hand asks for it after the deploy.
#[test]
fn production_moves_its_traffic_to_the_new_revision() {
    let fixture = Fixture::new();

    let output = fixture.deploy("kiewer", "0.3.2");

    assert_success(&output);
    assert_eq!(
        fixture.calls(),
        [
            deploy_call("kiewer", "0.3.2"),
            "run services update-traffic kiewer --project a-project \
             --region europe-west4 --to-latest --quiet"
                .to_owned(),
            describe_call("kiewer"),
        ],
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), DESCRIBED);
}

/// Staging never takes a candidate, so its traffic is never pinned.
#[test]
fn staging_deploys_and_shows_its_traffic() {
    let fixture = Fixture::new();

    let output = fixture.deploy("kiewer-staging", "1a2b3c4");

    assert_success(&output);
    assert_eq!(
        fixture.calls(),
        [
            deploy_call("kiewer-staging", "1a2b3c4"),
            describe_call("kiewer-staging"),
        ],
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), DESCRIBED);
}

#[test]
fn a_refused_traffic_move_fails_the_run() {
    let fixture = Fixture::new();
    fs::write(fixture.file("refuse-update-traffic"), "").expect("the refusal");

    let output = fixture.deploy("kiewer", "v0.3.2");

    assert!(!output.status.success());
    assert_eq!(fixture.calls().len(), 2, "calls: {:?}", fixture.calls());
}
