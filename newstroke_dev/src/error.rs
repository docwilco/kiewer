//! The one error type of the converter, and the exit code each kind maps
//! to.

use std::path::PathBuf;

use derive_more::{Display, Error, From};

/// Every way the converter fails.
#[derive(Debug, Display, Error, From)]
pub enum Error {
    /// The command line is not `<source dir> <output file>`.
    #[display("{_0}")]
    #[from(ignore)]
    Usage(#[error(not(source))] String),
    /// A source file could not be read.
    #[display("reading {}: {source}", path.display())]
    #[from(ignore)]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The output could not be written.
    #[display("writing {}: {source}", path.display())]
    #[from(ignore)]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A source file says something the converter does not read: the
    /// message names the file, the line and what was expected.
    #[display("{_0}")]
    #[from(ignore)]
    Malformed(#[error(not(source))] String),
}

/// The converter's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The process exit code for each kind of failure. Explicit discriminants
/// from 3, so a duplicate is a compile error; 0, 1, 2, 101 and 126 up are
/// never assigned (`Exit::reserved`, which a test checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    /// The command line.
    Usage = 3,
    /// Reading a source or writing the output.
    Io = 4,
    /// A source file the converter does not read.
    Malformed = 5,
}

impl Exit {
    /// Every kind, in code order.
    #[cfg(test)]
    pub const ALL: [Exit; 3] = [Exit::Usage, Exit::Io, Exit::Malformed];

    /// The process exit code.
    #[must_use]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Whether `code` is one the convention never assigns: 0 and 1 are
    /// success and a generic failure, 2 is a usage error in the shell's
    /// vocabulary, 101 is a Rust panic, 126 and up are the shell's.
    #[cfg(test)]
    pub fn reserved(code: u8) -> bool {
        matches!(code, 0..=2 | 101 | 126..)
    }
}

impl Error {
    /// The exit code for this failure; no wildcard, so a new variant does
    /// not compile until it has a kind.
    #[must_use]
    pub fn exit_code(&self) -> Exit {
        match self {
            Error::Usage(_) => Exit::Usage,
            Error::Read { .. } | Error::Write { .. } => Exit::Io,
            Error::Malformed(_) => Exit::Malformed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_exit_code_is_reserved() {
        for exit in Exit::ALL {
            assert!(!Exit::reserved(exit.code()), "{exit:?}");
        }
    }
}
