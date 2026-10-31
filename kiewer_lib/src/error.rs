//! The one error type of the library and the executable over it, and the
//! exit code each kind maps to.

use std::path::PathBuf;

use derive_more::{Display, Error, From};

use crate::account::Handle;
use crate::auth::Provider;
use crate::blob::BlobHash;

/// Every way the library fails. Variants are failure kinds with context,
/// never one per call site.
#[derive(Debug, Display, Error, From)]
pub enum Error {
    /// A plain I/O failure with no better context.
    #[display("{_0}")]
    Io(std::io::Error),
    /// Writing a blob into the store failed at this path.
    #[display("writing blob {hash} at {}: {source}", path.display())]
    #[from(ignore)]
    WriteBlob {
        hash: BlobHash,
        path: PathBuf,
        source: std::io::Error,
    },
    /// Reading a blob from the store failed at this path.
    #[display("reading blob {hash} at {}: {source}", path.display())]
    #[from(ignore)]
    ReadBlob {
        hash: BlobHash,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The bytes read under a hash do not hash to it: the store is
    /// damaged or someone wrote around it.
    #[display("blob {hash} is corrupt: its bytes hash to {actual}")]
    #[from(ignore)]
    CorruptBlob { hash: BlobHash, actual: BlobHash },
    /// A text that does not parse (a blob hash that is not 64 hex
    /// characters). The message is the value: nothing matches on which
    /// expectation failed.
    #[display("{_0}")]
    #[from(ignore)]
    Malformed(#[error(not(source))] String),
    /// An environment variable the server needs is missing or malformed.
    #[display("{name} {reason}")]
    #[from(ignore)]
    Env { name: &'static str, reason: String },
    /// Postgres: a connection or a query.
    #[display("database: {_0}")]
    Db(sqlx::Error),
    /// A migration failed, or one that ran has since been edited.
    #[display("migration: {_0}")]
    Migrate(sqlx::migrate::MigrateError),
    /// A page template did not render.
    #[display("template: {_0}")]
    Template(askama::Error),
    /// Another account already has this handle.
    #[display("handle {_0} is taken")]
    #[from(ignore)]
    HandleTaken(#[error(not(source))] Handle),
    /// A sign-in provider refused the exchange or answered without what
    /// the flow needs.
    #[display("{provider} sign-in: {reason}")]
    #[from(ignore)]
    OAuth { provider: Provider, reason: String },
    /// An HTTP call to a provider or another service failed.
    #[display("http: {_0}")]
    Http(reqwest::Error),
    /// A store that failed for a reason of its own (a test double, a
    /// remote service).
    #[display("{reason}")]
    #[from(ignore)]
    Failed { reason: String },
}

/// The library's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The process exit code for each kind of failure. Explicit discriminants
/// from 3, so a duplicate is a compile error; 0, 1, 2, 101 and 126 up are
/// never assigned ([`Exit::reserved`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    /// I/O, including the blob store's files.
    Io = 3,
    /// A blob that is corrupt or a hash that does not parse.
    Blob = 4,
    /// The environment is missing or malformed.
    Config = 5,
    /// Postgres, including migrations.
    Db = 6,
    /// A page did not render.
    Template = 7,
    /// A failure with no better kind.
    Failed = 8,
    /// A sign-in provider or another service over HTTP.
    Upstream = 9,
}

impl Exit {
    /// Every kind, in code order.
    pub const ALL: [Exit; 7] = [
        Exit::Io,
        Exit::Blob,
        Exit::Config,
        Exit::Db,
        Exit::Template,
        Exit::Failed,
        Exit::Upstream,
    ];

    /// The codes never assigned: 0 (success), 1 and 2 (the shell's and a
    /// usage error), 101 (a Rust panic) and 126 up (the shell's own).
    #[must_use]
    pub fn reserved(code: u8) -> bool {
        matches!(code, 0..=2 | 101 | 126..)
    }

    /// The code as a process exit status.
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// What each kind means, for the table.
    #[must_use]
    pub fn meaning(self) -> &'static str {
        match self {
            Exit::Io => "I/O, including the blob store's files",
            Exit::Blob => "a blob that is corrupt or a hash that does not parse",
            Exit::Config => "the environment is missing or malformed",
            Exit::Db => "Postgres, including migrations",
            Exit::Template => "a page did not render",
            Exit::Failed => "a failure with no better kind",
            Exit::Upstream => "a sign-in provider or another service over HTTP",
        }
    }

    /// The exit codes as a markdown table, the block README.md carries; a
    /// test keeps the two equal.
    #[must_use]
    pub fn table() -> String {
        use std::fmt::Write as _;
        let mut out = String::from("| Code | Kind | Meaning |\n|---|---|---|\n");
        for exit in Exit::ALL {
            // Writing to a String cannot fail.
            let _ = writeln!(out, "| {} | {exit:?} | {} |", exit.code(), exit.meaning());
        }
        out
    }
}

impl Error {
    /// The exit code for this failure. One `match` with no wildcard: a new
    /// variant does not compile until it has a kind.
    #[must_use]
    pub fn exit_code(&self) -> Exit {
        match self {
            Error::Io(_) | Error::WriteBlob { .. } | Error::ReadBlob { .. } => Exit::Io,
            Error::CorruptBlob { .. } | Error::Malformed(_) => Exit::Blob,
            Error::Env { .. } => Exit::Config,
            Error::Db(_) | Error::Migrate(_) => Exit::Db,
            Error::Template(_) => Exit::Template,
            Error::Failed { .. } | Error::HandleTaken(_) => Exit::Failed,
            Error::OAuth { .. } | Error::Http(_) => Exit::Upstream,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_exit_code_is_reserved() {
        for exit in Exit::ALL {
            assert!(!Exit::reserved(exit as u8), "{exit:?} uses a reserved code");
        }
    }

    #[test]
    fn the_readme_carries_the_table() {
        let readme = include_str!("../../README.md");
        assert!(
            readme.contains(&Exit::table()),
            "README.md's exit-code table is out of date; it should read:\n{}",
            Exit::table()
        );
    }

    #[test]
    fn all_lists_every_kind_once() {
        let mut codes: Vec<u8> = Exit::ALL.iter().map(|exit| *exit as u8).collect();
        codes.dedup();
        assert_eq!(codes.len(), Exit::ALL.len());
        assert!(
            codes.windows(2).all(|pair| pair[0] < pair[1]),
            "in code order"
        );
    }
}
