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

/// The variables the gates and the stage skips read, removed before each
/// run puts back its own: a run inside the hook or on CI has them set.
const GATE_VARS: &[&str] = &[
    "DATABASE_URL",
    "KIEWER_GCS_EMULATOR",
    "KIEWER_SKIP_DB_TESTS",
    "KIEWER_SKIP_GCS_TESTS",
    "CI",
    "CHECKS_SKIP",
];

/// The script's stages these tests run.
#[derive(Debug, Clone, Copy)]
enum Stage {
    Test,
    All,
}

impl Stage {
    fn arg(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::All => "all",
        }
    }
}

/// A service the gated tests need, by its variable and its opt-out.
#[derive(Debug, Clone, Copy)]
enum Gate {
    Postgres,
    Gcs,
}

impl Gate {
    fn var(self) -> &'static str {
        match self {
            Self::Postgres => "DATABASE_URL",
            Self::Gcs => "KIEWER_GCS_EMULATOR",
        }
    }

    /// A value for the variable; the stand-in cargo never connects to it.
    fn value(self) -> &'static str {
        match self {
            Self::Postgres => "postgres://kiewer:kiewer@127.0.0.1:5432/kiewer",
            Self::Gcs => "http://127.0.0.1:8092",
        }
    }

    fn opt_out(self) -> &'static str {
        match self {
            Self::Postgres => "KIEWER_SKIP_DB_TESTS",
            Self::Gcs => "KIEWER_SKIP_GCS_TESTS",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Postgres => Self::Gcs,
            Self::Gcs => Self::Postgres,
        }
    }

    fn set(self) -> (&'static str, &'static str) {
        (self.var(), self.value())
    }
}

struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    fn cargo_ran(&self) -> bool {
        self.stdout.contains("cargo-standin ")
    }

    /// The run failed at a gate, before any stage built anything.
    fn assert_refused_before_building(&self) {
        assert!(!self.success, "stdout: {}", self.stdout);
        assert!(!self.cargo_ran(), "stdout: {}", self.stdout);
    }

    fn assert_stderr_names(&self, text: &str) {
        assert!(
            self.stderr.contains(text),
            "{text} in stderr: {}",
            self.stderr
        );
    }
}

fn checks(stage: Stage, vars: &[(&str, &str)]) -> Run {
    let mut command = Command::new("bash");
    command.arg(SCRIPT).arg(stage.arg());
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
    let run = checks(Stage::Test, &[Gate::Postgres.set(), Gate::Gcs.set()]);
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

#[test_case(Gate::Postgres ; "postgres")]
#[test_case(Gate::Gcs ; "gcs")]
fn without_its_service_the_test_stage_fails_before_building(gate: Gate) {
    let run = checks(Stage::Test, &[gate.other().set()]);
    run.assert_refused_before_building();
    run.assert_stderr_names(gate.var());
    run.assert_stderr_names(&format!("{}=1", gate.opt_out()));
}

#[test]
fn an_empty_database_url_counts_as_unset() {
    let run = checks(Stage::Test, &[("DATABASE_URL", ""), Gate::Gcs.set()]);
    run.assert_refused_before_building();
    run.assert_stderr_names("KIEWER_SKIP_DB_TESTS=1");
}

#[test]
fn without_either_service_both_gates_are_named() {
    let run = checks(Stage::Test, &[]);
    run.assert_refused_before_building();
    run.assert_stderr_names("KIEWER_SKIP_DB_TESTS=1");
    run.assert_stderr_names("KIEWER_SKIP_GCS_TESTS=1");
}

#[test]
fn all_fails_before_the_first_stage_without_database_url() {
    let run = checks(Stage::All, &[Gate::Gcs.set()]);
    run.assert_refused_before_building();
    assert!(!run.stdout.contains("==> fmt"), "stdout: {}", run.stdout);
}

#[test_case(Gate::Postgres, &["db.rs", "accounts.rs", "collector.rs"] ; "postgres")]
#[test_case(Gate::Gcs, &["gcs_blob_store.rs"] ; "gcs")]
fn an_opt_out_runs_the_stage_and_names_what_skipped_last(gate: Gate, named: &[&str]) {
    let run = checks(Stage::Test, &[gate.other().set(), (gate.opt_out(), "1")]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(run.cargo_ran(), "stdout: {}", run.stdout);
    let last = run.stderr.trim_end().lines().last().unwrap_or_default();
    let opt_out = format!("{}=1", gate.opt_out());
    assert!(last.contains(&opt_out), "{opt_out} in: {last}");
    for name in named {
        assert!(last.contains(name), "{name} in: {last}");
    }
}

#[test_case(Gate::Postgres ; "postgres")]
#[test_case(Gate::Gcs ; "gcs")]
fn ci_refuses_an_opt_out(gate: Gate) {
    let run = checks(
        Stage::Test,
        &[gate.other().set(), (gate.opt_out(), "1"), ("CI", "true")],
    );
    run.assert_refused_before_building();
    run.assert_stderr_names(gate.opt_out());
}

/// The stages a commit's corners leave out: the three whose real scripts
/// the stand-in cargo cannot stand in for.
const SKIPPED: (&str, &str) = ("CHECKS_SKIP", "sqlx wasm browser");

#[test]
fn all_runs_every_stage_but_the_skipped_and_names_each_skip() {
    let run = checks(
        Stage::All,
        &[Gate::Postgres.set(), Gate::Gcs.set(), SKIPPED],
    );
    assert!(run.success, "stderr: {}", run.stderr);
    for stage in ["sqlx", "wasm", "browser"] {
        let skip = format!("==> {stage} skipped");
        assert!(
            run.stdout.contains(&skip),
            "{skip} in stdout: {}",
            run.stdout
        );
    }
    for stage in ["fmt", "clippy", "test", "docs"] {
        let ok = format!("==> {stage} ok");
        assert!(run.stdout.contains(&ok), "{ok} in stdout: {}", run.stdout);
    }
}

#[test]
fn ci_refuses_a_stage_skip() {
    let run = checks(
        Stage::All,
        &[
            Gate::Postgres.set(),
            Gate::Gcs.set(),
            SKIPPED,
            ("CI", "true"),
        ],
    );
    run.assert_refused_before_building();
    run.assert_stderr_names("CHECKS_SKIP");
}
