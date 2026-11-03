//! The server's log: which subscriber each [`LogFormat`] installs, built
//! over a writer so a test reads the bytes the server would write.

use kiewer_lib::config::LogFormat;
use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

/// Installs the log of `format` on stdout, under `RUST_LOG` or else
/// `info`. `terminal` says whether stdout is one, which colours the text.
pub fn init(format: LogFormat, terminal: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Json => install(json(filter, std::io::stdout)),
        LogFormat::Text => install(text(filter, terminal, std::io::stdout)),
    }
}

/// `set_global_default` and not tracing-subscriber's `init`: `init` also
/// routes the `log` crate into the subscriber whenever tracing-subscriber's
/// `tracing-log` feature is on, and tracing-stackdriver's dependency on
/// tracing-subscriber turns it on. wgpu, naga and reqwest log through
/// `log`, so `init` would add their lines to both formats.
fn install(subscriber: impl Subscriber + Send + Sync + 'static) {
    tracing::subscriber::set_global_default(subscriber)
        .expect("the log is installed once, first thing in main");
}

/// One JSON object per line in the shape Cloud Logging reads: `severity`,
/// `time` (RFC 3339), `message`, `target` and the source location under
/// `logging.googleapis.com/sourceLocation`, with each event field a
/// top-level key of the jsonPayload Cloud Logging makes of the line. The
/// layer camelCases those keys as it writes them (`user_agent_family` is
/// `userAgentFamily`), and a log-based metric's extractor names the
/// written key (`infra/logging.tf`).
fn json<W>(filter: EnvFilter, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_stackdriver::layer().with_writer(writer))
}

/// One line per event for a person reading a terminal.
fn text<W>(filter: EnvFilter, ansi: bool, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_writer(writer)
        .finish()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use test_case::test_case;

    use super::*;

    /// A writer that keeps its bytes for the test to read.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("the buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("the buffer").clone()).expect("UTF-8")
        }

        fn maker(&self) -> impl Fn() -> Buffer + Send + Sync + 'static {
            let buffer = self.clone();
            move || buffer.clone()
        }
    }

    fn filter() -> EnvFilter {
        EnvFilter::new("info")
    }

    /// The text line of one event: its timestamp's shape is checked here
    /// and the bytes after it are the caller's to compare, since they are
    /// all a run can pin.
    fn text_after_the_timestamp(ansi: bool) -> String {
        let buffer = Buffer::default();
        tracing::subscriber::with_default(text(filter(), ansi, buffer.maker()), || {
            tracing::info!(count = 3, net = %"GND", "pad moved");
        });
        let line = buffer.contents();
        let (time, rest) = line.split_once(' ').expect("a timestamp and a line");
        let time = time
            .trim_start_matches("\u{1b}[2m")
            .trim_end_matches("\u{1b}[0m");
        assert_eq!(time.len(), "2026-11-03T12:00:00.000000Z".len(), "{time}");
        assert_eq!(&time[10..11], "T", "{time}");
        assert!(time.ends_with('Z'), "{time}");
        rest.to_owned()
    }

    #[test_case(false, " INFO kiewer::log::tests: pad moved count=3 net=GND\n" ; "plain")]
    #[test_case(true, "\u{1b}[32m INFO\u{1b}[0m \u{1b}[2mkiewer::log::tests\u{1b}[0m\u{1b}[2m:\u{1b}[0m pad moved \u{1b}[3mcount\u{1b}[0m\u{1b}[2m=\u{1b}[0m3 \u{1b}[3mnet\u{1b}[0m\u{1b}[2m=\u{1b}[0mGND\n" ; "coloured")]
    fn a_text_line_keeps_its_bytes(ansi: bool, expected: &str) {
        assert_eq!(text_after_the_timestamp(ansi), expected);
    }

    /// The one JSON line `event` makes.
    fn json_line(event: impl FnOnce()) -> serde_json::Value {
        let buffer = Buffer::default();
        tracing::subscriber::with_default(json(filter(), buffer.maker()), event);
        serde_json::from_str(&buffer.contents()).expect("one JSON object")
    }

    #[test]
    fn an_error_event_is_severity_error_at_its_time() {
        let line = json_line(|| tracing::error!(error = "no database", "request failed"));
        assert_eq!(line["severity"], "ERROR", "{line}");
        let time = line["time"].as_str().expect("a time string");
        assert_eq!(&time[10..11], "T", "RFC 3339: {time}");
        assert!(time.ends_with('Z'), "RFC 3339 in UTC: {time}");
        assert!(line.get("level").is_none(), "{line}");
        assert!(line.get("timestamp").is_none(), "{line}");
    }

    #[test]
    fn a_warning_is_the_severity_cloud_logging_spells_warning() {
        let line = json_line(|| tracing::warn!("the purge failed"));
        assert_eq!(line["severity"], "WARNING", "{line}");
    }

    #[test]
    fn an_info_event_is_severity_info() {
        let line = json_line(|| tracing::info!("listening"));
        assert_eq!(line["severity"], "INFO", "{line}");
    }

    #[test]
    fn target_and_message_stay_where_the_beacon_metric_filters_on_them() {
        let line = json_line(|| tracing::info!("renderer beacon"));
        assert_eq!(line["target"], "kiewer::log::tests", "{line}");
        assert_eq!(line["message"], "renderer beacon", "{line}");
    }

    #[test]
    fn a_field_is_written_under_its_camel_cased_name() {
        let line = json_line(|| tracing::info!(user_agent_family = "chrome", "renderer beacon"));
        assert_eq!(line["userAgentFamily"], "chrome", "{line}");
        assert!(line.get("user_agent_family").is_none(), "{line}");
    }

    /// The one test that installs the process's global subscriber; every
    /// other test scopes its own to its thread.
    #[test]
    fn the_log_crate_is_not_routed_into_the_log() {
        init(LogFormat::Text, false);
        assert_eq!(::log::max_level(), ::log::LevelFilter::Off);
    }

    #[test]
    fn an_event_names_its_source_where_cloud_logging_reads_it() {
        let line = json_line(|| tracing::info!("listening"));
        let location = &line["logging.googleapis.com/sourceLocation"];
        assert_eq!(location["file"], "kiewer/src/log.rs", "{line}");
    }
}
