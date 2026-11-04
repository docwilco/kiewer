//! The integration tests of `kiewer_lib`, one module per area, in one test
//! binary. Each file directly under `tests/` would be a crate of its own
//! and link an executable of its own, statically, carrying the crate and
//! all its dependencies with their debuginfo: a copy per file, and a new
//! set of copies for every change to a dependency list or a feature set
//! (#100). A new area is a file in this directory and its `mod` line
//! here. One area runs as
//! `cargo build --workspace --all-targets && cargo test --workspace --test it <module>::`.
//! `kicad_core` has an `it` binary too, so a module name both have
//! (`schematic`) runs in both; the filter names the path, never `-p`
//! (rust-conventions).

mod accounts;
mod beacon;
mod branches;
mod browser;
mod cache;
mod collector;
mod db;
mod delete;
mod design_settings;
mod designs;
mod files;
mod forks;
mod fs_blob_store;
mod gcs_blob_store;
mod history;
mod pending;
mod prune_images;
mod roles;
mod schematic;
mod sign_in;
mod slugs;
mod takedowns;
mod thumbnails;
mod upload;
mod viewer;
mod web;
