//! The filesystem blob store against the shared suite, under cargo's
//! scratch directory.

use kiewer_lib::blob::BlobStore;
use kiewer_lib::blob::fs::FsBlobStore;
use kiewer_test_support::{TempDir, blob_store_suite as suite, scratch_dir};

/// An empty store under cargo's scratch directory, which lives as long as
/// the returned guard.
fn store() -> (FsBlobStore, TempDir) {
    let dir = scratch_dir!();
    (FsBlobStore::new(dir.path()), dir)
}

#[tokio::test]
async fn round_trips() {
    let (store, _dir) = store();
    suite::round_trips(&store).await;
}

#[tokio::test]
async fn deduplicates_by_hash() {
    let (store, _dir) = store();
    suite::deduplicates_by_hash(&store).await;
}

#[tokio::test]
async fn reports_a_missing_blob() {
    let (store, _dir) = store();
    suite::reports_a_missing_blob(&store).await;
}

#[tokio::test]
async fn names_a_blob_by_the_sha256_of_its_bytes() {
    let (store, _dir) = store();
    suite::names_a_blob_by_the_sha256_of_its_bytes(&store).await;
}

/// The store's own invariant beyond the trait: the bytes on disk are
/// gzip under the hash (ADR 0002), one file per blob, so a second put
/// writes nothing.
#[tokio::test]
async fn keeps_one_gzip_file_per_blob() {
    let (store, dir) = store();
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

/// Many tasks putting the same bytes at once each get the hash back, and
/// the directory ends with one whole file and no temporary left behind.
#[tokio::test]
async fn concurrent_puts_of_one_blob_leave_one_file() {
    let (store, dir) = store();
    let bytes = b"(kicad_pcb (version 20241229) (generator \"kiewer\"))".repeat(64);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let store = store.clone();
        let bytes = bytes.clone();
        tasks.spawn(async move { store.put(&bytes).await });
    }
    let expected = kiewer_lib::blob::BlobHash::of(&bytes);
    while let Some(joined) = tasks.join_next().await {
        let receipt = joined.expect("task").expect("every put succeeds");
        assert_eq!(receipt.hash, expected);
    }
    let files = walkdir(dir.path());
    assert_eq!(files.len(), 1, "one file, no temporaries: {files:?}");
    assert_eq!(
        store.get(&expected).await.expect("get").as_deref(),
        Some(bytes.as_slice())
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
