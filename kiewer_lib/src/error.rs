//! The one error type of the library and the executable over it, and the
//! exit code each kind maps to.

use std::path::PathBuf;

use derive_more::{Display, Error, From};

use crate::account::Handle;
use crate::auth::Provider;
use crate::blob::BlobHash;
use crate::blob::gcs::BlobOp;
use crate::roles::Role;

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
    /// Deleting a blob from the store failed at this path or object.
    #[display("deleting blob {hash} at {}: {reason}", path.display())]
    #[from(ignore)]
    DeleteBlob {
        hash: BlobHash,
        path: PathBuf,
        reason: String,
    },
    /// GCS refused or failed an operation on a blob: the bucket, the
    /// credentials or the network.
    #[display("{op} blob {hash} in bucket {bucket}: {source}")]
    #[from(ignore)]
    Gcs {
        op: BlobOp,
        hash: BlobHash,
        bucket: String,
        source: google_cloud_storage::Error,
    },
    /// The GCS client for a bucket could not be built: no credentials
    /// found, or a configuration refused.
    #[display("the GCS store for bucket {bucket}: {source}")]
    #[from(ignore)]
    GcsClient {
        bucket: String,
        source: google_cloud_gax::client_builder::Error,
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
    /// The thumbnail renderer: no Vulkan driver, a file that will not
    /// draw, or a frame that would not read back (ADR 0004).
    #[display("thumbnail: {_0}")]
    Render(kiewer_viewer::error::Error),
    /// Another account already has this handle.
    #[display("handle {_0} is taken")]
    #[from(ignore)]
    HandleTaken(#[error(not(source))] Handle),
    /// The handle was given up with its account's delete, and nobody may
    /// take it until the reservation ends (`account::choose_handle`).
    #[display("handle {_0} is reserved")]
    #[from(ignore)]
    HandleReserved(#[error(not(source))] Handle),
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
    /// An upload the server would not take, for a reason the uploader can
    /// act on: the upload page shows it, with a status of the web layer's
    /// choosing.
    #[display("upload refused: {_0}")]
    Refused(#[error(not(source))] Refusal),
    /// The signed-in person's role on the design is below what the write
    /// needs, or they have none (`roles::require`); the web layer answers
    /// 403 with this message.
    #[display("this needs the {needed} role on the design")]
    #[from(ignore)]
    Forbidden { needed: Role },
}

/// Why an upload was refused (docs/v1-scope.md, Uploads). The message is
/// shown to the uploader as it is, so it names the file and the limit.
#[derive(Debug, Clone, PartialEq, Eq, Display)]
pub enum Refusal {
    /// One file is over the per-file cap.
    #[display("{path} is larger than {limit_mb} MB, the limit for one file")]
    FileTooLarge { path: String, limit_mb: u64 },
    /// The files together are over the per-revision cap.
    #[display("the files together are larger than {limit_mb} MB, the limit for one revision")]
    RevisionTooLarge { limit_mb: u64 },
    /// The whole body, the dropped files included, is over the
    /// upload route's body limit (`upload::BODY_LIMIT`). `limit_mb` is the
    /// per-revision cap the limit is built on, not the limit itself: the
    /// framing allowance on top is the server's, not the uploader's.
    #[display(
        "the upload is larger than {limit_mb} MB in all; leave out files that are not KiCad files"
    )]
    UploadTooLarge { limit_mb: u64 },
    /// The account's designs would hold more than the per-account cap.
    #[display("this upload would take the account over {limit_gb} GB of stored files")]
    AccountFull { limit_gb: u64 },
    /// Nothing in the upload is a KiCad file.
    #[display("no KiCad file: upload a .kicad_pcb, .kicad_sch or .kicad_pro")]
    NoKicadFile,
    /// A KiCad-named file does not parse; `error` is the parser's message.
    #[display("{path}: {error}")]
    DoesNotParse { path: String, error: String },
    /// Parsing the files took longer than the server allows.
    #[display("parsing took longer than {seconds} seconds")]
    ParseTimeout { seconds: u64 },
    /// A blob the operator has blocked site-wide.
    #[display("{path} is a file this site does not accept")]
    Blocked { path: String },
    /// A new design's slug is one the owner already has.
    #[display("you already have a design at {slug}; give this one another name")]
    SlugTaken { slug: String },
    /// The upload names a design that does not exist; `slug` is the path
    /// as the form carried it (`handle/slug`).
    #[display("there is no design {slug}")]
    NoSuchDesign { slug: String },
    /// The settings form was built for a design at `slug`, and the design
    /// is no longer there: someone moved it while the form was open.
    #[display("the design is no longer at {slug}; open its page again and retry")]
    SlugMoved { slug: String },
    /// The upload names a branch the design does not have.
    #[display("the design has no branch {branch}")]
    NoSuchBranch { branch: String },
    /// A new branch's name is one the design already has.
    #[display("the design already has a branch {branch}; give the new one another name")]
    BranchTaken { branch: String },
    /// A new branch was to start at a revision the design does not reach.
    #[display("the design has no revision {hash}")]
    NoSuchRevision { hash: String },
    /// A new branch's name is missing or breaks the rules.
    #[display("{reason}")]
    BadBranchName { reason: String },
    /// The branch's head is not the one this write was built on: a
    /// re-append whose page showed an older head (`crate::pending`), or
    /// the compare-and-set under a push (`design::set_head`), which turns
    /// a moved head into a pending revision before it gets that far.
    #[display("the branch {branch} has moved again; look at where it is now and choose")]
    HeadMoved { branch: String },
    /// The default branch cannot go while it is the default.
    #[display("{branch} is the default branch; make another branch the default first")]
    DefaultBranch { branch: String },
    /// The multipart body itself could not be read.
    #[display("the upload could not be read: {reason}")]
    Unreadable { reason: String },
    /// A zip uploaded by itself: the upload page expands a zip and sends
    /// its files (ADR 0005), so this one came from a form sent with
    /// scripts off, and the server reads no zip.
    #[display(
        "{path}: a zip needs the upload page with scripts on; or choose the project folder or its files"
    )]
    ZipPart { path: String },
    /// The account has made its hourly allowance of uploads; the web
    /// layer answers 429 with `retry_after_secs` as `Retry-After`.
    #[display(
        "you have made {limit} uploads in the last hour, the limit, refused ones included; try again in {retry_after_secs} seconds"
    )]
    RateLimited { limit: u32, retry_after_secs: u64 },
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
            Error::Io(_)
            | Error::WriteBlob { .. }
            | Error::ReadBlob { .. }
            | Error::DeleteBlob { .. }
            | Error::Gcs { .. } => Exit::Io,
            Error::CorruptBlob { .. } | Error::Malformed(_) => Exit::Blob,
            Error::Env { .. } | Error::GcsClient { .. } => Exit::Config,
            Error::Db(_) | Error::Migrate(_) => Exit::Db,
            Error::Template(_) => Exit::Template,
            Error::Failed { .. }
            | Error::HandleTaken(_)
            | Error::HandleReserved(_)
            | Error::Refused(_)
            | Error::Forbidden { .. }
            | Error::Render(_) => Exit::Failed,
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
