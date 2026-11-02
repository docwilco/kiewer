//! The kiewer server executable: the build line, the log, then the
//! library's server. Every non-zero exit is an `Exit` code from the
//! library's one error type.

use std::io::IsTerminal;
use std::process::ExitCode;

use kiewer_lib::config::{Config, LogFormat};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // The build line first, on stdout, before anything can fail
    // (release-conventions): a deploy that dies at startup still says
    // what it was.
    println!("{}", kiewer_lib::build::line());
    let terminal = std::io::stdout().is_terminal();
    let format = match LogFormat::from_env(terminal) {
        Ok(format) => format,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(error.exit_code().code());
        }
    };
    let log = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    );
    match format {
        // Flattened, so an event's fields are top-level keys of the
        // jsonPayload Cloud Logging makes of the line, where a log-based
        // metric's extractor reads them (infra/logging.tf).
        LogFormat::Json => log.json().flatten_event(true).init(),
        LogFormat::Text => log.with_ansi(terminal).init(),
    }
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
