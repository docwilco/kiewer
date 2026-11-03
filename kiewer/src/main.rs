//! The kiewer server executable: the build line, the log, then the
//! library's server. Every non-zero exit is an `Exit` code from the
//! library's one error type.

mod log;

use std::io::IsTerminal;
use std::process::ExitCode;

use kiewer_lib::config::{Config, LogFormat};

#[tokio::main]
async fn main() -> ExitCode {
    // The build line first, on stdout, before anything can fail
    // (release-conventions): a deploy that dies at startup still says
    // what it was.
    println!("{}", kiewer_lib::build::line());
    let terminal = std::io::stdout().is_terminal();
    let format = match LogFormat::from_env(terminal) {
        Ok(format) => format,
        Err(error) => return failed(&error),
    };
    log::init(format, terminal);
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => failed(&error),
    }
}

/// The one line on stderr and the exit code of `error`'s kind.
fn failed(error: &kiewer_lib::Error) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::from(error.exit_code().code())
}

async fn run() -> kiewer_lib::Result<()> {
    let config = Config::from_env()?;
    kiewer_lib::server::run(config).await
}
