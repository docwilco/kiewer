//! The one error type of the library and the executable over it.

use std::path::PathBuf;

use derive_more::{Display, Error, From};

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
    /// A text that should have been a blob hash: 64 hex characters.
    #[display("not a blob hash: {_0:?}")]
    #[from(ignore)]
    MalformedHash(#[error(not(source))] String),
    /// A store that failed for a reason of its own (a test double, a
    /// remote service).
    #[display("{reason}")]
    #[from(ignore)]
    Failed { reason: String },
}

/// The library's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
