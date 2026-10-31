//! Parser, model and geometry for KiCad board and schematic files.
//! Compiles to wasm32 for the viewer and natively for the server.
//!
//! [`parse_board`] reads a KiCad 9 or 10 `.kicad_pcb` into a
//! [`model::Board`]. Elements the model has no place for (groups,
//! dimensions, 3D models, plot settings) are skipped; a known element with
//! a missing or malformed field, an unknown layer name, or a format older
//! than KiCad 9 is an [`Error`], never a panic.

// A parser of untrusted input: an index the input puts out of bounds must
// fail as a parse error and never as a panic, so slicing and indexing are
// written as checked lookups here and nowhere else in the workspace.
#![warn(clippy::indexing_slicing, clippy::string_slice)]

pub mod board;
pub mod error;
pub mod model;
pub mod sexpr;

pub use board::parse_board;
pub use error::{Error, MIN_BOARD_VERSION, Result};
