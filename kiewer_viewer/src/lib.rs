//! The browser viewer: draws a parsed design on a canvas with wgpu.
//!
//! Built for `wasm32-unknown-unknown` by `scripts/build-viewer.sh` and
//! loaded by the viewer page; the same crate links natively for tests
//! and the thumbnail renderer (ADR 0004). [`camera`] is the view in f64,
//! [`scene`] the board as triangles, [`render`] the wgpu pipeline at the
//! WebGL2 floor; the canvas and the browser's events are `web`, compiled
//! for wasm32 alone.

use wasm_bindgen::prelude::wasm_bindgen;

pub mod camera;
pub mod render;
pub mod scene;
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
