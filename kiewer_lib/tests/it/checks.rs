//! `scripts/checks.sh`, the test stage's gates (#68): the tests that need
//! Postgres or the GCS emulator return early without their variable and
//! count as passed, so a run without one fails unless its opt-out is set,
//! and an opt-out says loudly what skipped.
//!
//! The script is the workspace's own. Cargo is a stand-in that prints its
//! arguments, so the stage builds nothing: a bash function exported into
//! the script's environment, which shadows the cargo on PATH. A function
//! and not a file on PATH, so no test writes an executable and then runs
//! it while another test thread's fork may hold it open (ETXTBSY, #108).

use std::process::{Command, Output};

use test_case::test_case;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/checks.sh");
const DB: &str = "postgres://kiewer:kiewer@127.0.0.1:5432/kiewer";
const GCS: &str = "http://127.0.0.1:8092";

/// The variables the gates read, removed before each run puts back its
/// own: a run inside the hook or on CI has them set.
const GATE_VARS: &[&str] = &[
    "DATABASE_URL",
    "KIEWER_GCS_EMULATOR",
    "KIEWER_SKIP_DB_TESTS",
    "KIEWER_SKIP_GCS_TESTS",
    "CI",
];

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    fn cargo_ran(&self) -> bool {
        self.stdout.contains("cargo-standin ")
    }
}

fn checks(stage: &str, vars: &[(&str, &str)]) -> Run {
    let mut command = Command::new("bash");
    command.arg(SCRIPT).arg(stage);
    for var in GATE_VARS {
        command.env_remove(var);
    }
    command.env("BASH_FUNC_cargo%%", "() { echo \"cargo-standin $*\"; }");
    command.envs(vars.iter().copied());
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().expect("bash runs the script");
    Run {
        success: status.success(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

#[test]
fn with_both_services_the_test_stage_runs_and_names_no_skip() {
    let run = checks(
        "test",
        &[("DATABASE_URL", DB), ("KIEWER_GCS_EMULATOR", GCS)],
    );
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(
        run.stdout
            .contains("cargo-standin build --workspace --all-targets"),
        "stdout: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("cargo-standin test --workspace"),
        "stdout: {}",
        run.stdout
    );
    assert!(!run.stderr.contains("skip"), "stderr: {}", run.stderr);
}

#[test_case(&[("KIEWER_GCS_EMULATOR", GCS)] ; "unset")]
#[test_case(&[("DATABASE_URL", ""), ("KIEWER_GCS_EMULATOR", GCS)] ; "empty")]
fn without_database_url_the_test_stage_fails_before_building(vars: &[(&str, &str)]) {
    let run = checks("test", vars);
    assert!(!run.success, "stdout: {}", run.stdout);
    assert!(!run.cargo_ran(), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("DATABASE_URL"),
        "stderr: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("KIEWER_SKIP_DB_TESTS=1"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn without_the_gcs_emulator_the_test_stage_fails_before_building() {
    let run = checks("test", &[("DATABASE_URL", DB)]);
    assert!(!run.success, "stdout: {}", run.stdout);
    assert!(!run.cargo_ran(), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("KIEWER_SKIP_GCS_TESTS=1"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn without_either_service_both_gates_are_named() {
    let run = checks("test", &[]);
    assert!(!run.success, "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("KIEWER_SKIP_DB_TESTS=1")
            && run.stderr.contains("KIEWER_SKIP_GCS_TESTS=1"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn every_stage_fails_at_once_without_database_url() {
    let run = checks("all", &[("KIEWER_GCS_EMULATOR", GCS)]);
    assert!(!run.success, "stdout: {}", run.stdout);
    assert!(!run.stdout.contains("==> fmt"), "stdout: {}", run.stdout);
}

#[test_case("KIEWER_SKIP_DB_TESTS", &[("KIEWER_GCS_EMULATOR", GCS)], &["db.rs", "accounts", "collector"] ; "postgres")]
#[test_case("KIEWER_SKIP_GCS_TESTS", &[("DATABASE_URL", DB)], &["gcs_blob_store.rs"] ; "gcs")]
fn an_opt_out_runs_the_stage_and_names_what_skipped_last(
    opt_out: &str,
    vars: &[(&str, &str)],
    named: &[&str],
) {
    let mut vars = vars.to_vec();
    vars.push((opt_out, "1"));
    let run = checks("test", &vars);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(run.cargo_ran(), "stdout: {}", run.stdout);
    let last = run.stderr.trim_end().lines().last().unwrap_or_default();
    assert!(
        last.contains(&format!("{opt_out}=1")),
        "stderr: {}",
        run.stderr
    );
    for name in named {
        assert!(last.contains(name), "{name} in: {last}");
    }
}

#[test_case("KIEWER_SKIP_DB_TESTS", &[("KIEWER_GCS_EMULATOR", GCS)] ; "postgres")]
#[test_case("KIEWER_SKIP_GCS_TESTS", &[("DATABASE_URL", DB)] ; "gcs")]
fn ci_refuses_an_opt_out(opt_out: &str, vars: &[(&str, &str)]) {
    let mut vars = vars.to_vec();
    vars.extend([(opt_out, "1"), ("CI", "true")]);
    let run = checks("test", &vars);
    assert!(!run.success, "stdout: {}", run.stdout);
    assert!(!run.cargo_ran(), "stdout: {}", run.stdout);
    assert!(run.stderr.contains(opt_out), "stderr: {}", run.stderr);
}
