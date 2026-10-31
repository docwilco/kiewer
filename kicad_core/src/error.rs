//! The one error type of the crate.

use derive_more::{Display, Error};

/// The oldest board format this crate reads: KiCad 9.0's `20241229`.
/// Everything older is refused with [`Error::UnsupportedVersion`].
pub const MIN_BOARD_VERSION: u32 = 20_241_229;

/// Why a file did not become a model.
///
/// Every failure names where in the text it was found (1-based line and
/// byte offset) and what was expected there; a malformed file is never a
/// panic.
#[derive(Debug, Clone, PartialEq, Eq, Display, Error)]
pub enum Error {
    /// The text is not a board this crate accepts: an unbalanced
    /// S-expression, a missing field, a bad number, an unknown layer.
    #[display("line {line} (byte {offset}): {message}")]
    Malformed {
        /// Byte offset into the source where the failure was found.
        offset: usize,
        /// 1-based line of that offset.
        line: u32,
        /// What was expected, and what was found instead when known.
        message: String,
    },
    /// The file was written by a KiCad older than 9.0.
    #[display(
        "board format version {version} is older than KiCad 9 (version {} and later); \
         open the board in KiCad 9 or later and save it again",
        MIN_BOARD_VERSION
    )]
    UnsupportedVersion {
        /// The `(version N)` the file carries.
        version: u32,
    },
}

/// The crate's `Result`.
pub type Result<T, E = Error> = core::result::Result<T, E>;
