//! What the workspace's tests share: cargo's scratch directory and the
//! behaviour every blob store has to show.

pub use tempfile::{TempDir, tempdir_in};

pub mod blob_store_suite;

/// A fresh directory under cargo's scratch directory, removed on drop.
///
/// A macro and not a function: cargo sets `CARGO_TARGET_TMPDIR` only while
/// it compiles an integration test or a bench, so `env!` has to expand
/// inside the test itself.
#[macro_export]
macro_rules! scratch_dir {
    () => {
        $crate::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("cargo's scratch directory is writable")
    };
}
