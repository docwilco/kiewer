//! Blobs: the bytes of one file, named by the SHA-256 of the uncompressed
//! bytes and stored gzip-compressed (ADR 0002). The server computes every
//! hash from bytes it holds; a store never trusts a hash from outside.
//!
//! [`BlobStore`] is the seam between the server and where the bytes live:
//! [`fs::FsBlobStore`] for development and tests, [`gcs::GcsBlobStore`] in
//! production. Both keep the same bytes under the same names, so the
//! shared suite in `kiewer_test_support` holds for each.

pub mod fs;
pub mod gcs;

use std::io::{Read, Write};

use async_trait::async_trait;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::hex::sha256_name;

sha256_name!(
    BlobHash,
    "blob hash",
    "The SHA-256 of a blob's uncompressed bytes: 64 lowercase hex characters in text."
);

impl BlobHash {
    /// The hash of these bytes.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

/// What a `put` found: whether the bytes were new to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// The store did not have the blob and wrote it.
    Stored,
    /// The store already had the blob under this hash and wrote nothing.
    Existed,
}

/// What a `put` hands back: the blob's hash and whether it was new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutReceipt {
    pub hash: BlobHash,
    pub outcome: Put,
}

/// Where blobs live. Object-safe, so the server holds an
/// `Arc<dyn BlobStore>` and the environment picks the implementation.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Store these bytes under their own hash. A blob already present is
    /// left as it is and reported as [`Put::Existed`].
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt>;

    /// The uncompressed bytes under this hash, or `None` when the store
    /// has no such blob.
    ///
    /// # Errors
    ///
    /// When the store cannot read, or the bytes it holds do not hash to
    /// `hash` ([`Error::CorruptBlob`]).
    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>>;

    /// Whether the store holds a blob under this hash.
    ///
    /// # Errors
    ///
    /// When the store cannot answer.
    async fn exists(&self, hash: &BlobHash) -> Result<bool>;

    /// The stored form of the blob under this hash, one gzip member of
    /// the bytes, or `None` when the store has no such blob: what the
    /// blob route sends with `Content-Encoding: gzip`, so a serve costs
    /// no compression. Not checked against `hash` here, since that takes
    /// the bytes out again; [`verified`] over [`gunzip`] is the caller's.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>>;
}

/// The stored form of a blob: one gzip member of the bytes.
///
/// # Errors
///
/// Only when the in-memory writer fails, which it does not.
pub fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// The bytes back out of their stored form.
///
/// # Errors
///
/// When `compressed` is not a gzip member.
pub fn gunzip(compressed: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    GzDecoder::new(compressed).read_to_end(&mut out)?;
    Ok(out)
}

/// The name a blob is stored under, in every store: `<first two hex>/<hash>.gz`,
/// a path under the filesystem store's root and an object key in a bucket,
/// so a directory and a bucket hold the same bytes under the same names,
/// and Fastly's rewrite (infra/fastly/recv.vcl.tftpl) finds them.
#[must_use]
pub fn object_name(hash: &BlobHash) -> String {
    let name = hash.to_string();
    format!("{}/{name}.gz", &name[..2])
}

/// Run a store's blocking work (hashing, compression, the filesystem) off
/// the runtime's threads. A task that does not finish (a panic in it, a
/// runtime shutting down) is a failure of the store, reported, never a
/// panic of the caller.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join| Error::Failed {
            reason: format!("the blob store's blocking task did not finish: {join}"),
        })?
}

/// The bytes read back under `hash`, checked against it. Every store calls
/// this on a `get`, so a damaged object fails loudly rather than rendering.
///
/// # Errors
///
/// [`Error::CorruptBlob`] when the bytes hash to something else.
pub fn verified(hash: &BlobHash, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let actual = BlobHash::of(&bytes);
    if actual == *hash {
        Ok(bytes)
    } else {
        Err(Error::CorruptBlob {
            hash: *hash,
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test]
    fn hash_round_trips_through_text() {
        let hash = BlobHash::of(b"kiewer");
        let text = hash.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<BlobHash>().expect("parses"), hash);
    }

    #[test_case("abc", false; "too short")]
    #[test_case("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", false; "not hex")]
    #[test_case("abababababababababababababababababababababababababababababababab", true; "64 hex")]
    fn hash_text_must_be_64_hex_characters(text: &str, parses: bool) {
        assert_eq!(text.parse::<BlobHash>().is_ok(), parses);
    }

    #[test]
    fn objects_are_named_by_their_hash_under_a_two_hex_shard() {
        let hash = BlobHash::of(b"kiewer");
        assert_eq!(
            object_name(&hash),
            "ad/ad8e964fa89208513d804735d67ba5e70ac9154e3658b8ffd9eea31853860ad2.gz"
        );
    }

    #[test]
    fn gzip_round_trips() {
        let bytes = b"(kicad_pcb)".repeat(100);
        let compressed = gzip(&bytes).expect("gzip");
        assert!(compressed.len() < bytes.len());
        assert_eq!(gunzip(&compressed).expect("gunzip"), bytes);
    }

    #[test]
    fn verified_rejects_other_bytes() {
        let hash = BlobHash::of(b"kiewer");
        assert!(matches!(
            verified(&hash, b"not kiewer".to_vec()),
            Err(Error::CorruptBlob { .. })
        ));
    }
}
