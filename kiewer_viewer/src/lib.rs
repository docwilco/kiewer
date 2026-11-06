//! The browser viewer: draws a parsed design on a canvas with wgpu.
//!
//! Built for `wasm32-unknown-unknown` by `scripts/build-viewer.sh` and
//! loaded by the viewer page; the same crate links natively for tests
//! and the thumbnail renderer (ADR 0004). [`camera`] is the view in f64,
//! [`scene`] the board as triangles, [`schematic`] a sheet as triangles,
//! [`render`] the wgpu pipeline at the
//! WebGL2 floor, [`error`] the one error type; the canvas and the
//! browser's events are `web`, compiled for wasm32 alone.

use wasm_bindgen::prelude::wasm_bindgen;

// The browser tests' probe (Cargo.toml, panic-probe) makes a panic on
// purpose; a build that could ship never has it.
#[cfg(all(feature = "panic-probe", not(debug_assertions)))]
compile_error!("panic-probe is for the browser tests' debug build alone, never a release build");

pub mod atlas;
pub mod camera;
pub mod error;
#[cfg(not(target_arch = "wasm32"))]
pub mod headless;
pub mod highlight;
pub mod labels;
pub mod pick;
pub mod refine;
pub mod render;
pub mod scene;
pub mod schematic;
pub mod sheet_highlight;
pub mod view;
#[cfg(target_arch = "wasm32")]
pub mod web;

/// The viewer's Version, so a page can tell which build it loaded.
#[wasm_bindgen]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_the_workspace_version() {
        assert_eq!(super::version(), env!("CARGO_PKG_VERSION"));
    }
}
