//! The behaviour every blob store shows, seen through the trait alone. An
//! integration test for a store calls each function against a fresh,
//! empty store: the filesystem store in `kiewer_lib/tests/`, the GCS
//! store in its own test.

use kiewer_lib::blob::{BlobHash, BlobStore, Put, gunzip};

/// What goes in comes back out, by the hash `put` handed back.
///
/// # Panics
///
/// When the store misbehaves: that is the assertion.
pub async fn round_trips(store: &dyn BlobStore) {
    let bytes = b"(kicad_pcb (version 20241229))".as_slice();
    let hash = store.put(bytes).await.expect("put").hash;
    assert!(store.exists(&hash).await.expect("exists"));
    let read = store
        .get(&hash)
        .await
        .expect("get")
        .expect("the blob just put is there");
    assert_eq!(read, bytes);
}

/// The same bytes put twice are one blob: the second `put` reports a
/// duplicate under the same hash, and the blob reads back once.
///
/// # Panics
///
/// When the store misbehaves: that is the assertion.
pub async fn deduplicates_by_hash(store: &dyn BlobStore) {
    let bytes = b"(kicad_sch (version 20250114))".as_slice();
    let first = store.put(bytes).await.expect("first put");
    let second = store.put(bytes).await.expect("second put");
    assert_eq!(first.outcome, Put::Stored);
    assert_eq!(second.outcome, Put::Existed);
    assert_eq!(first.hash, second.hash);
    let other = store
        .put(b"(kicad_sch (version 20250115))")
        .await
        .expect("other put");
    assert_eq!(other.outcome, Put::Stored);
    assert_ne!(other.hash, first.hash);
}

/// The stored form is one gzip member of the bytes, so a route can send
/// it as it is with `Content-Encoding: gzip`; a hash nothing was put under
/// has none.
///
/// # Panics
///
/// When the store misbehaves: that is the assertion.
pub async fn hands_out_the_stored_gzip_form(store: &dyn BlobStore) {
    let bytes = b"(kicad_pro)".repeat(50);
    let hash = store.put(&bytes).await.expect("put").hash;
    let stored = store
        .get_gzip(&hash)
        .await
        .expect("get_gzip")
        .expect("the blob just put is there");
    assert!(stored.starts_with(&[0x1f, 0x8b]), "the gzip magic");
    assert!(stored.len() < bytes.len(), "compressed");
    assert_eq!(gunzip(&stored).expect("a gzip member"), bytes);
    assert!(
        store
            .get_gzip(&BlobHash::of(b"never put"))
            .await
            .expect("get_gzip")
            .is_none()
    );
}

/// A hash nothing was put under is absent, not an error.
///
/// # Panics
///
/// When the store misbehaves: that is the assertion.
pub async fn reports_a_missing_blob(store: &dyn BlobStore) {
    let hash = BlobHash::of(b"never put");
    assert!(!store.exists(&hash).await.expect("exists"));
    assert!(store.get(&hash).await.expect("get").is_none());
}

/// A blob is named by the SHA-256 of its uncompressed bytes (ADR 0002),
/// so a hash computed anywhere else finds it. The literal is
/// `printf kiewer | sha256sum`.
///
/// # Panics
///
/// When the store misbehaves: that is the assertion.
pub async fn names_a_blob_by_the_sha256_of_its_bytes(store: &dyn BlobStore) {
    let hash = store.put(b"kiewer").await.expect("put").hash;
    assert_eq!(
        hash.to_string(),
        "ad8e964fa89208513d804735d67ba5e70ac9154e3658b8ffd9eea31853860ad2"
    );
    let parsed: BlobHash = "ad8e964fa89208513d804735d67ba5e70ac9154e3658b8ffd9eea31853860ad2"
        .parse()
        .expect("64 hex characters parse");
    assert_eq!(
        store.get(&parsed).await.expect("get").as_deref(),
        Some(b"kiewer".as_slice())
    );
}
