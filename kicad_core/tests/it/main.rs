//! The integration tests of `kicad_core`, one module per area, in one test
//! binary. Each file directly under `tests/` would be a crate of its own
//! and link an executable of its own, statically, carrying the crate and
//! all its dependencies with their debuginfo: a copy per file, and a new
//! set of copies for every change to a dependency list or a feature set
//! (#100). A new area is a file in this directory and its `mod` line
//! here. One area runs as
//! `cargo build --workspace --all-targets && cargo test --workspace --test it <module>::`.

mod board;
mod connect;
mod malformed;
mod schematic;
mod schematic_tessellate;
mod sheet_hit;
mod sheet_tree;
mod tessellate;
