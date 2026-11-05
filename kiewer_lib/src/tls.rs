//! The process's rustls crypto provider.
//!
//! The Google crates (google-cloud-storage, google-cloud-auth) are built
//! without their default feature, which would bring aws-lc-rs and its C
//! build (`kiewer_lib/Cargo.toml` says why), and so read the provider from
//! the process: reqwest 0.13, which google-cloud-auth builds its clients
//! on (the metadata server's token calls among them), panics at a client's
//! build with none installed, and google-cloud-auth's signing with a
//! service account key panics too. Ring is the one provider in the tree:
//! reqwest 0.12, oauth2 and sqlx bring it, and use it without an install.

/// Make ring the process's provider, unless one is installed already.
/// Every constructor of a Google client calls it first, so no order of
/// construction (the GCS store, the Cloud Tasks queue, a test of either)
/// reaches a client without one.
pub(crate) fn install_crypto_provider() {
    // Err only means a provider is installed already, which is the aim.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
