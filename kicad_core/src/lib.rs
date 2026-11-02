//! Parser, model and geometry for KiCad board and schematic files.
//! Compiles to wasm32 for the viewer and natively for the server.
//!
//! [`parse_board`] reads a KiCad 9 or 10 `.kicad_pcb` into a
//! [`model::Board`]; [`parse_schematic`] reads a `.kicad_sch` of the same
//! generations into a [`schematic::model::Schematic`], its symbols from the
//! file's own `lib_symbols`. [`tessellate::Tessellator`] turns a board's
//! copper and outline into triangles, [`tessellate::schematic`] a sheet's
//! symbols, wires and labels. Elements the models have no place for
//! (groups, dimensions, 3D models, plot settings, images) are skipped; a
//! known element with a missing or malformed field, an unknown layer name,
//! or a format older than KiCad 9 is an [`Error`], never a panic.

// A parser of untrusted input: an index the input puts out of bounds must
// fail as a parse error and never as a panic, so slicing and indexing are
// written as checked lookups here and nowhere else in the workspace.
#![warn(clippy::indexing_slicing, clippy::string_slice)]

pub mod board;
pub mod error;
pub mod font;
pub mod geometry;
pub mod model;
pub mod schematic;
pub mod sexpr;
pub mod tessellate;

pub use board::parse_board;
pub use error::{Error, MIN_BOARD_VERSION, MIN_SCHEMATIC_VERSION, Result};
pub use schematic::parse_schematic;
