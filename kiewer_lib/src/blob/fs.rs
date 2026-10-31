//! The filesystem blob store: development and tests. One gzip file per
//! blob at `<root>/<first two hex>/<hash>.gz`, the same object layout the
//! GCS store uses, so a directory and a bucket hold the same bytes under
//! the same names.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{BlobHash, BlobStore, Put, Stored, gunzip, gzip, verified};
use crate::error::{Error, Result};

/// Blobs under one directory. Cheap to clone: a path.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// A store rooted at `root`, created on the first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory the store writes under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, hash: &BlobHash) -> PathBuf {
        let name = hash.to_string();
        self.root.join(&name[..2]).join(format!("{name}.gz"))
    }

    fn put_sync(&self, bytes: &[u8]) -> Result<Stored> {
        let hash = BlobHash::of(bytes);
        let path = self.path_of(&hash);
        if path.exists() {
            return Ok(Stored {
                hash,
                outcome: Put::Existed,
            });
        }
        let write = || -> std::io::Result<()> {
            let dir = path
                .parent()
                .expect("a blob path has its shard directory above it");
            std::fs::create_dir_all(dir)?;
            // Written beside its final name and renamed into place, so a
            // reader never sees a partial file and two writers of the same
            // blob both end with one whole one.
            let tmp = dir.join(format!(".{}.{}.tmp", hash, std::process::id()));
            std::fs::write(&tmp, gzip(bytes)?)?;
            std::fs::rename(&tmp, &path)
        };
        write().map_err(|source| Error::WriteBlob { hash, path, source })?;
        Ok(Stored {
            hash,
            outcome: Put::Stored,
        })
    }

    fn get_sync(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let path = self.path_of(hash);
        let compressed = match std::fs::read(&path) {
            Ok(compressed) => compressed,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::ReadBlob {
                    hash: *hash,
                    path,
                    source,
                });
            }
        };
        let bytes = gunzip(&compressed).map_err(|source| Error::ReadBlob {
            hash: *hash,
            path,
            source,
        })?;
        verified(hash, bytes).map(Some)
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<Stored> {
        // Compression and the write are blocking work; off the runtime's
        // threads. The bytes are copied once for the move into the task.
        let store = self.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || store.put_sync(&bytes))
            .await
            .expect("the blocking put neither panics nor is cancelled")
    }

    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let store = self.clone();
        let hash = *hash;
        tokio::task::spawn_blocking(move || store.get_sync(&hash))
            .await
            .expect("the blocking get neither panics nor is cancelled")
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        let path = self.path_of(hash);
        tokio::task::spawn_blocking(move || path.exists())
            .await
            .map_err(|join| Error::Failed {
                reason: format!("the blocking exists check did not finish: {join}"),
            })
    }
}
