//! The kiewer server executable: the build line, the log, then the
//! library's server. Every non-zero exit is an `Exit` code from the
//! library's one error type.

use std::process::ExitCode;

use kiewer_lib::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // The build line first, on stdout, before anything can fail
    // (release-conventions): a deploy that dies at startup still says
    // what it was.
    println!("{}", kiewer_lib::build::line());
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code().code())
        }
    }
}

async fn run() -> kiewer_lib::Result<()> {
    let config = Config::from_env()?;
    kiewer_lib::server::run(config).await
}
