//! The GCS blob store against the shared suite, on a GCS emulator
//! (fake-gcs-server) named by `KIEWER_GCS_EMULATOR`, the base URL of its
//! JSON API, for example `http://127.0.0.1:8092`. Each test makes a bucket
//! of its own on the emulator, so each runs against a fresh, empty store.
//! Without the variable every test here returns at once with a line
//! saying so: a machine without the emulator skips the store, it does not
//! fail it. CI starts the emulator and sets the variable
//! (.github/workflows/ci.yml, the test job); the README's Development
//! section says how to on a dev box.

use std::sync::atomic::{AtomicUsize, Ordering};

use kiewer_lib::blob::BlobStore;
use kiewer_lib::blob::gcs::GcsBlobStore;
use kiewer_test_support::blob_store_suite as suite;

const ENDPOINT: &str = "KIEWER_GCS_EMULATOR";

/// A number no two buckets of this process share; the pid keeps two
/// processes apart.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A store on a bucket nothing else uses, and what the test needs to look
/// at the bucket behind it through the emulator's JSON API.
struct Fixture {
    store: GcsBlobStore,
    endpoint: String,
    bucket: String,
    http: reqwest::Client,
}

/// The fixture, or `None` with a line on stderr when there is no emulator
/// to make it on.
async fn fresh(test: &str) -> Option<Fixture> {
    let Ok(endpoint) = std::env::var(ENDPOINT) else {
        eprintln!("{ENDPOINT} is not set; {test} skips the GCS store");
        return None;
    };
    let bucket = format!(
        "kiewer-test-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        test.replace('_', "-")
    );
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{endpoint}/storage/v1/b?project=kiewer-test"))
        .header("content-type", "application/json")
        .body(format!("{{\"name\":\"{bucket}\"}}"))
        .send()
        .await
        .expect("the emulator answers a bucket insert");
    assert!(
        response.status().is_success(),
        "making bucket {bucket} on {endpoint}: {}",
        response.status()
    );
    let store = GcsBlobStore::emulated(&bucket, &endpoint)
        .await
        .expect("a store on the emulator");
    Some(Fixture {
        store,
        endpoint,
        bucket,
        http,
    })
}

impl Fixture {
    /// Every object in the bucket, as the JSON API lists them.
    async fn objects(&self) -> Vec<serde_json::Value> {
        let listing: serde_json::Value = self
            .http
            .get(format!("{}/storage/v1/b/{}/o", self.endpoint, self.bucket))
            .send()
            .await
            .expect("the emulator lists the bucket")
            .json()
            .await
            .expect("a JSON listing");
        listing["items"].as_array().cloned().unwrap_or_default()
    }

    /// An object's stored bytes, asked for as stored (gzip accepted).
    async fn stored_bytes(&self, name: &str) -> Vec<u8> {
        self.http
            .get(format!(
                "{}/storage/v1/b/{}/o/{}?alt=media",
                self.endpoint,
                self.bucket,
                name.replace('/', "%2F")
            ))
            .header("accept-encoding", "gzip")
            .send()
            .await
            .expect("the emulator serves the object")
            .bytes()
            .await
            .expect("the object's bytes")
            .to_vec()
    }
}

#[tokio::test]
async fn round_trips() {
    let Some(f) = fresh("round_trips").await else {
        return;
    };
    suite::round_trips(&f.store).await;
}

#[tokio::test]
async fn deduplicates_by_hash() {
    let Some(f) = fresh("deduplicates_by_hash").await else {
        return;
    };
    suite::deduplicates_by_hash(&f.store).await;
}

#[tokio::test]
async fn reports_a_missing_blob() {
    let Some(f) = fresh("reports_a_missing_blob").await else {
        return;
    };
    suite::reports_a_missing_blob(&f.store).await;
}

#[tokio::test]
async fn names_a_blob_by_the_sha256_of_its_bytes() {
    let Some(f) = fresh("names_a_blob_by_the_sha256_of_its_bytes").await else {
        return;
    };
    suite::names_a_blob_by_the_sha256_of_its_bytes(&f.store).await;
}

#[tokio::test]
async fn hands_out_the_stored_gzip_form() {
    let Some(f) = fresh("hands_out_the_stored_gzip_form").await else {
        return;
    };
    suite::hands_out_the_stored_gzip_form(&f.store).await;
}

/// The store's own invariant beyond the trait: one object per blob at
/// `<first two hex>/<hash>.gz`, the layout the filesystem store and the
/// Fastly rewrite (infra/fastly/recv.vcl.tftpl) spell, holding gzip bytes
/// marked `Content-Encoding: gzip` (ADR 0002), so a second put writes
/// nothing and a reader through Fastly gets bytes it can decode.
#[tokio::test]
async fn keeps_one_gzip_object_per_blob() {
    let Some(f) = fresh("keeps_one_gzip_object_per_blob").await else {
        return;
    };
    let hash = f.store.put(b"kiewer").await.expect("put").hash;
    f.store.put(b"kiewer").await.expect("second put");
    let name = hash.to_string();

    let objects = f.objects().await;
    assert_eq!(objects.len(), 1, "one object for one blob: {objects:?}");
    let object = &objects[0];
    assert_eq!(object["name"], format!("{}/{name}.gz", &name[..2]));
    assert_eq!(object["contentEncoding"], "gzip");

    let stored = f
        .stored_bytes(object["name"].as_str().expect("a name"))
        .await;
    assert_eq!(
        &stored[..2],
        &[0x1f, 0x8b],
        "a gzip member starts with its magic bytes"
    );
}
