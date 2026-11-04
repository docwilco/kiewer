//! The one error type of the crate.

use derive_more::{Display, Error};

/// The oldest board format this crate reads: KiCad 9.0's `20241229`.
/// Everything older is refused with [`Error::UnsupportedVersion`].
pub const MIN_BOARD_VERSION: u32 = 20_241_229;

/// The oldest schematic format this crate reads: KiCad 9.0's `20250114`.
/// Everything older is refused with [`Error::UnsupportedSchematicVersion`].
pub const MIN_SCHEMATIC_VERSION: u32 = 20_250_114;

/// Why a file did not become a model.
///
/// Every failure in the text names where it was found (1-based line and
/// byte offset) and what was expected there; a malformed file is never a
/// panic. The position is why `Malformed` carries fields rather than
/// being the conventions' bare `Malformed(String)`: a 2 MB board with one
/// bad number is only diagnosable by line, and the callers that show the
/// error (the upload page, the viewer) want the line as a value, not
/// parsed back out of the message.
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
    /// The schematic was written by a KiCad older than 9.0.
    #[display(
        "schematic format version {version} is older than KiCad 9 (version {} and later); \
         open the schematic in KiCad 9 or later and save it again",
        MIN_SCHEMATIC_VERSION
    )]
    UnsupportedSchematicVersion {
        /// The `(version N)` the file carries.
        version: u32,
    },
    /// The parse was given a deadline (`parse_board_before`,
    /// `parse_schematic_before`) and ran past it; it stopped there rather
    /// than run to the end.
    #[display("the parse ran past its deadline")]
    DeadlinePassed,
}

/// The crate's `Result`.
pub type Result<T, E = Error> = core::result::Result<T, E>;
