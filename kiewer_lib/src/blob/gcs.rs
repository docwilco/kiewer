//! The GCS blob store: production. One object per blob at
//! `<first two hex>/<hash>.gz` in a private bucket, the layout the
//! filesystem store uses and the Fastly rewrite spells
//! (infra/fastly/recv.vcl.tftpl), holding the gzip bytes with
//! `Content-Encoding: gzip`, so Fastly and the browser decode them and the
//! bytes travel compressed end to end (ADR 0002, ADR 0003).
//!
//! Credentials come from where the process runs: the runtime service
//! account through the metadata server on Cloud Run, application-default
//! credentials on a dev box (`gcloud auth application-default login`), or
//! a key file named by `GOOGLE_APPLICATION_CREDENTIALS`. An emulator takes
//! none: [`GcsBlobStore::emulated`].

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_storage::builder::storage::ClientBuilder;
use google_cloud_storage::client::Storage;
use google_cloud_storage::model_ext::ReadRange;

use super::{BlobHash, BlobStore, Put, PutReceipt, blocking, gunzip, gzip, object_name, verified};
use crate::error::{Error, Result};

/// What the store was doing when GCS failed it; the `op` of
/// [`Error::Gcs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOp {
    Put,
    Get,
    Exists,
}

impl fmt::Display for BlobOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BlobOp::Put => "putting",
            BlobOp::Get => "getting",
            BlobOp::Exists => "checking",
        })
    }
}

/// The status GCS answers a precondition that did not hold, here a create
/// of an object that exists.
const PRECONDITION_FAILED: u16 = 412;
/// The status GCS answers for an object that is not there.
const NOT_FOUND: u16 = 404;

/// Blobs in one bucket. Cheap to clone: the client is shared.
#[derive(Debug, Clone)]
pub struct GcsBlobStore {
    bucket: String,
    /// The bucket as the client names it: `projects/_/buckets/<bucket>`.
    resource: String,
    client: Storage,
}

impl GcsBlobStore {
    /// A store on this bucket, with the credentials of the process (the
    /// module doc says which).
    ///
    /// # Errors
    ///
    /// [`Error::GcsClient`] when no credentials can be found or the
    /// configuration is refused. The bucket itself is not touched until the
    /// first operation.
    pub async fn new(bucket: &str) -> Result<Self> {
        Self::build(bucket, Storage::builder()).await
    }

    /// A store on this bucket of a GCS emulator (fake-gcs-server) at
    /// `endpoint`, the base URL of its JSON API, with no credentials sent.
    /// For tests.
    ///
    /// # Errors
    ///
    /// [`Error::GcsClient`] when the configuration is refused.
    pub async fn emulated(bucket: &str, endpoint: &str) -> Result<Self> {
        let anonymous = google_cloud_auth::credentials::anonymous::Builder::new().build();
        let builder = Storage::builder()
            .with_endpoint(endpoint)
            .with_credentials(anonymous);
        Self::build(bucket, builder).await
    }

    async fn build(bucket: &str, builder: ClientBuilder) -> Result<Self> {
        let client = builder.build().await.map_err(|source| Error::GcsClient {
            bucket: bucket.to_owned(),
            source,
        })?;
        Ok(Self {
            bucket: bucket.to_owned(),
            resource: format!("projects/_/buckets/{bucket}"),
            client,
        })
    }

    /// The bucket the store reads and writes.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    fn failed(&self, op: BlobOp, hash: BlobHash, source: google_cloud_storage::Error) -> Error {
        Error::Gcs {
            op,
            hash,
            bucket: self.bucket.clone(),
            source,
        }
    }

    /// The `gs://` URL of a blob's object, for an error that names it.
    fn url_of(&self, object: &str) -> PathBuf {
        PathBuf::from(format!("gs://{}/{object}", self.bucket))
    }
}

#[async_trait]
impl BlobStore for GcsBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<PutReceipt> {
        // Hashing and compression are CPU work; off the runtime's threads.
        // The bytes are copied once for the move into the task.
        let owned = bytes.to_vec();
        let (hash, compressed) = blocking(move || {
            let hash = BlobHash::of(&owned);
            let compressed = gzip(&owned)?;
            Ok((hash, compressed))
        })
        .await?;
        // A blob already there is the common case of a re-upload: one
        // metadata request settles it without sending the bytes. The write
        // is still a create (generation 0), not an overwrite, so two writers
        // racing past the check both end with one object and the loser
        // learns it from the 412.
        if self.exists(&hash).await? {
            return Ok(PutReceipt {
                hash,
                outcome: Put::Existed,
            });
        }
        let write = self
            .client
            .write_object(&self.resource, object_name(&hash), Bytes::from(compressed))
            .set_if_generation_match(0)
            .set_content_encoding("gzip")
            .set_content_type("application/octet-stream")
            // Content-addressed, so immutable; no-transform keeps GCS from
            // decompressing the object for a reader that does not say it
            // accepts gzip (the client here always does).
            .set_cache_control("public, max-age=31536000, immutable, no-transform");
        // The resumable protocol, with the checksums computed up front and
        // sent in the metadata. The single-shot upload the client would
        // otherwise pick for a small payload sends its checksum as a
        // trailing multipart part, which the emulator this store is tested
        // on appends to the object; GCS proper takes either. Pinned on the
        // heap: the upload's state machine is tens of kilobytes, too much
        // to carry on the stack of every caller's future.
        let written = Box::pin(write.send_buffered()).await;
        match written {
            Ok(_) => Ok(PutReceipt {
                hash,
                outcome: Put::Stored,
            }),
            Err(source) if source.http_status_code() == Some(PRECONDITION_FAILED) => {
                Ok(PutReceipt {
                    hash,
                    outcome: Put::Existed,
                })
            }
            Err(source) => Err(self.failed(BlobOp::Put, hash, source)),
        }
    }

    async fn get_gzip(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        // The client asks for gzip and gets the stored bytes, never a
        // transcoded copy: it says so in its docs, and the checksums it
        // verifies are of the stored bytes.
        let mut response = match self
            .client
            .read_object(&self.resource, object_name(hash))
            .send()
            .await
        {
            Ok(response) => response,
            Err(source) if source.http_status_code() == Some(NOT_FOUND) => return Ok(None),
            Err(source) => return Err(self.failed(BlobOp::Get, *hash, source)),
        };
        let mut compressed = Vec::new();
        while let Some(chunk) = response.next().await {
            let chunk = chunk.map_err(|source| self.failed(BlobOp::Get, *hash, source))?;
            compressed.extend_from_slice(&chunk);
        }
        Ok(Some(compressed))
    }

    async fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        let Some(compressed) = self.get_gzip(hash).await? else {
            return Ok(None);
        };
        let (hash, url) = (*hash, self.url_of(&object_name(hash)));
        let bytes = blocking(move || {
            gunzip(&compressed).map_err(|source| Error::ReadBlob {
                hash,
                path: url,
                source,
            })
        })
        .await?;
        verified(&hash, bytes).map(Some)
    }

    async fn exists(&self, hash: &BlobHash) -> Result<bool> {
        // The client has no metadata-only read over the JSON API (that is
        // the gRPC control client, which an emulator lacks), so this is a
        // read of the first byte: the status answers the question and one
        // byte is the cost.
        let read = self
            .client
            .read_object(&self.resource, object_name(hash))
            .set_read_range(ReadRange::head(1))
            .send()
            .await;
        match read {
            Ok(_) => Ok(true),
            Err(source) if source.http_status_code() == Some(NOT_FOUND) => Ok(false),
            Err(source) => Err(self.failed(BlobOp::Exists, *hash, source)),
        }
    }
}
