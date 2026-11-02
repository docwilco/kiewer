//! `/blob/{hash}`: a stored file's bytes, as the store holds them. The
//! stored form is one gzip member (ADR 0002), sent as it is with
//! `Content-Encoding: gzip` to a client that accepts gzip (every browser)
//! and decoded for one that does not, so `Vary: Accept-Encoding`. Content
//! addressed, so immutable: a year's `max-age` (`crate::cache`). The bytes are checked against the hash before they
//! leave, so a damaged object is a 500 in the log, never a broken render.
//! A hash that is not one or that nothing was stored under is 404. A
//! hash the operator has blocked is 410, as the file route answers for
//! the same blob (`web/files.rs`): #23 chose saying so over the 404 this
//! route gave at first. Kept briefly, since a takedown can be lifted
//! ([`Policy::NotFound`]). In production Fastly sends `/blob/{hash}` to
//! the bucket (infra/fastly/recv.vcl.tftpl), so this check never runs
//! there and a copy Fastly holds stays: that gap is #71.

use axum::extract::{Path, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE, VARY, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::AppState;
use crate::blob::{BlobHash, blocking, gunzip, verified};
use crate::cache::{self, Encoding, Policy};
use crate::design;
use crate::error::{Error, Result};

/// The route a blob is served at.
#[must_use]
pub fn url(hash: &BlobHash) -> String {
    format!("/blob/{hash}")
}

/// Every blob is one of the three KiCad text files the upload keeps.
const TEXT: &str = "text/plain; charset=utf-8";

fn not_found() -> Response {
    cache::not_found("no such blob\n")
}

/// The answer for a blocked blob, on either route that serves one.
pub(super) fn taken_down() -> Response {
    (
        StatusCode::GONE,
        [Policy::NotFound.header()],
        "this file was taken down\n",
    )
        .into_response()
}

/// `GET /blob/{hash}`.
///
/// # Errors
///
/// The database's, the store's, or [`Error::CorruptBlob`] when the stored
/// bytes do not hash to their name: a 500 with the cause in the log.
pub async fn serve(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let Ok(hash) = hash.parse::<BlobHash>() else {
        return Ok(not_found());
    };
    if design::is_blocked(&state.db, &hash).await? {
        return Ok(taken_down());
    }
    Ok(stored(&state, hash, &headers)
        .await?
        .unwrap_or_else(not_found))
}

/// The bytes stored under `hash` as a response: the stored gzip to a
/// client that takes it, decoded for one that does not, immutable either
/// way; `None` when nothing is stored under it. The caller has checked
/// the hash against `blocked_blob`.
///
/// # Errors
///
/// As [`serve`].
pub(super) async fn stored(
    state: &AppState,
    hash: BlobHash,
    headers: &HeaderMap,
) -> Result<Option<Response>> {
    let Some(compressed) = state.blobs.get_gzip(&hash).await? else {
        return Ok(None);
    };
    // Decoding and hashing are CPU work, off the runtime's threads. The
    // decoded bytes are the body for a client without gzip.
    let (compressed, bytes) = blocking(move || {
        let bytes = gunzip(&compressed).map_err(|source| Error::ReadBlob {
            hash,
            path: crate::blob::object_name(&hash).into(),
            source,
        })?;
        let bytes = verified(&hash, bytes)?;
        Ok((compressed, bytes))
    })
    .await?;
    let common = [
        (CONTENT_TYPE, TEXT),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        Policy::Immutable.header(),
        (VARY, "Accept-Encoding"),
    ];
    Ok(Some(if Encoding::accepts(headers, "gzip") {
        (common, [(CONTENT_ENCODING, "gzip")], compressed).into_response()
    } else {
        (common, bytes).into_response()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_url_is_its_hash_under_blob() {
        let hash = BlobHash::of(b"kiewer");
        assert_eq!(
            url(&hash),
            "/blob/ad8e964fa89208513d804735d67ba5e70ac9154e3658b8ffd9eea31853860ad2"
        );
    }
}
