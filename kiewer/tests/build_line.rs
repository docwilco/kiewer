//! The executable's first line on stdout is its build line, before it
//! reads its configuration: a deploy that fails at startup still says
//! what it was (release-conventions).

use std::process::Command;

use kiewer_lib::error::Exit;

#[test]
fn prints_the_build_line_first_then_fails_without_a_database_url() {
    let output = Command::new(env!("CARGO_BIN_EXE_kiewer"))
        .env_remove("DATABASE_URL")
        .env_remove("PORT")
        .output()
        .expect("the executable runs");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let first = stdout.lines().next().expect("a first line on stdout");
    let expected = format!(
        "kiewer {}+{} ({}, {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("KIEWER_BUILD").unwrap_or("dev"),
        kiewer_lib::build::TARGET,
        kiewer_lib::build::PROFILE,
    );
    assert_eq!(first, expected);
    assert_eq!(kiewer_lib::build::line(), expected);
    assert_eq!(
        output.status.code(),
        Some(i32::from(Exit::Config as u8)),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DATABASE_URL"),
        "names the missing variable: {stderr}"
    );
}
