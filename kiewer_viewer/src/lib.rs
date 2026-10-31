//! The browser viewer: draws a parsed design on a canvas with wgpu.
//!
//! Built for `wasm32-unknown-unknown` by `scripts/checks.sh wasm` and
//! loaded by the design page; the same crate links natively for tests
//! and the thumbnail renderer (ADR 0004).

use wasm_bindgen::prelude::wasm_bindgen;

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
