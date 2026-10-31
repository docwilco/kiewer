//! The filesystem blob store: development and tests. One gzip file per
//! blob at `<root>/<first two hex>/<hash>.gz`, the same object layout the
//! GCS store uses, so a directory and a bucket hold the same bytes under
//! the same names.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use super::{BlobHash, BlobStore, Put, PutReceipt, gunzip, gzip, verified};
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
        let name = hash.to_string();
        self.root.join(&name[..2]).join(format!("{name}.gz"))
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

    fn get_sync(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let path = self.path_of(hash);
        let read_blob = |source| Error::ReadBlob {
            hash: *hash,
            path: path.clone(),
            source,
        };
        let compressed = match std::fs::read(&path) {
            Ok(compressed) => compressed,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(read_blob(source)),
        };
        let bytes = gunzip(&compressed).map_err(read_blob)?;
        verified(hash, bytes).map(Some)
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

/// Run blocking filesystem work off the runtime's threads. A task that
/// does not finish (a panic in it, a runtime shutting down) is a failure
/// of the store, reported, never a panic of the caller.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|join| Error::Failed {
            reason: format!("the filesystem store's blocking task did not finish: {join}"),
        })?
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
}
