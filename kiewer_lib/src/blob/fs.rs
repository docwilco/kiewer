//! The filesystem blob store: development and tests. One gzip file per
//! blob at `<root>/<first two hex>/<hash>.gz`, the same object layout the
//! GCS store uses, so a directory and a bucket hold the same bytes under
//! the same names.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use super::{BlobHash, BlobStore, Put, PutReceipt, blocking, gunzip, gzip, object_name, verified};
use crate::error::{Error, Result};

/// Blobs under one directory. Cheap to clone: a path.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

/// A number no two writes in this process share, for the temporary file
/// a put fills before the rename; the pid keeps two processes apart.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

impl FsBlobStore {
    /// A store rooted at `root`, created on the first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_of(&self, hash: &BlobHash) -> PathBuf {
        self.root.join(object_name(hash))
    }

    fn put_sync(&self, bytes: &[u8]) -> Result<PutReceipt> {
        let hash = BlobHash::of(bytes);
        let path = self.path_of(&hash);
        if path.exists() {
            return Ok(PutReceipt {
                hash,
                outcome: Put::Existed,
            });
        }
        let dir = path
            .parent()
            .expect("a blob path has its shard directory above it");
        // Filled under a name of its own and renamed into place, so a reader
        // never sees a partial file. Two writers of the same blob each fill
        // their own file; the rename is atomic, so the second replaces the
        // first with identical bytes, and both report what they found.
        let tmp = dir.join(format!(
            ".{}.{}.{}.tmp",
            hash,
            std::process::id(),
            WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(dir)?;
            std::fs::write(&tmp, gzip(bytes)?)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(source) = write() {
            // Best effort: a failed write leaves no half-filled file behind.
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::WriteBlob { hash, path, source });
        }
        Ok(PutReceipt {
            hash,
            outcome: Put::Stored,
        })
    }

    fn get_gzip_sync(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let path = self.path_of(hash);
        match std::fs::read(&path) {
            Ok(compressed) => Ok(Some(compressed)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::ReadBlob {
                hash: *hash,
                path,
                source,
            }),
        }
    }

    fn get_sync(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let Some(compressed) = self.get_gzip_sync(hash)? else {
            return Ok(None);
        };
        let bytes = gunzip(&compressed).map_err(|source| Error::ReadBlob {
            hash: *hash,
            path: self.path_of(hash),
            source,
        })?;
        verified(hash, bytes).map(Some)
    }

    fn delete_sync(&self, hash: &BlobHash) -> Result<()> {
        let path = self.path_of(hash);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::DeleteBlob {
                hash: *hash,
                path,
                reason: source.to_string(),
            }),
        }
    }

    fn exists_sync(&self, hash: &BlobHash) -> Result<bool> {
        let path = self.path_of(hash);
        match std::fs::metadata(&path) {
            Ok(_) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::ReadBlob {
                hash: *hash,
                path,
                source,
            }),
        }
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        // The bytes are copied once for the move into the task.
        let store = self.clone();
        let bytes = bytes.to_vec();
        blocking(move || store.put_sync(&bytes)).await
    }

    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let store = self.clone();
        let hash = *hash;
        blocking(move || store.get_sync(&hash)).await
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        let store = self.clone();
        let hash = *hash;
        blocking(move || store.exists_sync(&hash)).await
    }

    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let store = self.clone();
        let hash = *hash;
        blocking(move || store.get_gzip_sync(&hash)).await
    }

    async fn delete(&self, hash: &BlobHash) -> Result<()> {
        let store = self.clone();
        let hash = *hash;
        blocking(move || store.delete_sync(&hash)).await
    }
}
