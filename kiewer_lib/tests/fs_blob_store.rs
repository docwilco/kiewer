//! The filesystem blob store against the shared suite, under cargo's
//! scratch directory.

use kiewer_lib::blob::BlobStore;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_test_support::{blob_store_suite as suite, scratch_dir};

#[tokio::test]
async fn round_trips() {
    let dir = scratch_dir!();
    suite::round_trips(&FsBlobStore::new(dir.path())).await;
}

#[tokio::test]
async fn deduplicates_by_hash() {
    let dir = scratch_dir!();
    suite::deduplicates_by_hash(&FsBlobStore::new(dir.path())).await;
}

#[tokio::test]
async fn reports_a_missing_blob() {
    let dir = scratch_dir!();
    suite::reports_a_missing_blob(&FsBlobStore::new(dir.path())).await;
}

#[tokio::test]
async fn names_a_blob_by_the_sha256_of_its_bytes() {
    let dir = scratch_dir!();
    suite::names_a_blob_by_the_sha256_of_its_bytes(&FsBlobStore::new(dir.path())).await;
}

/// The store's own invariant beyond the trait: the bytes on disk are
/// gzip under the hash (ADR 0002), one file per blob, so a second put
/// writes nothing.
#[tokio::test]
async fn keeps_one_gzip_file_per_blob() {
    let dir = scratch_dir!();
    let store = FsBlobStore::new(dir.path());
    let hash = store.put(b"kiewer").await.expect("put").hash;
    store.put(b"kiewer").await.expect("second put");
    let files: Vec<_> = walkdir(dir.path());
    assert_eq!(files.len(), 1, "one file for one blob: {files:?}");
    let name = files[0]
        .file_name()
        .expect("a file")
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, format!("{hash}.gz"));
    let on_disk = std::fs::read(&files[0]).expect("read");
    assert_eq!(
        &on_disk[..2],
        &[0x1f, 0x8b],
        "a gzip member starts with its magic bytes"
    );
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else {
            out.push(path);
        }
    }
    out
}
