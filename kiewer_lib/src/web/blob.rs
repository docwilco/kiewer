//! `/blob/{hash}`: a stored file's bytes, as the store holds them. The
//! stored form is one gzip member (ADR 0002), sent as it is with
//! `Content-Encoding: gzip` to a client that accepts gzip (every browser)
//! and decoded for one that does not, so `Vary: Accept-Encoding`. Content
//! addressed, so immutable: a year's `max-age` (the caching policy as a
//! whole is #26). The bytes are checked against the hash before they
//! leave, so a damaged object is a 500 in the log, never a broken render.
//! A hash that is not one, that nothing was stored under, or that the
//! operator has blocked is 404 alike: the route says nothing about which.

use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, VARY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::AppState;
use crate::blob::{BlobHash, blocking, gunzip, verified};
use crate::design;
use crate::error::{Error, Result};

/// The route a blob is served at.
#[must_use]
pub fn url(hash: &BlobHash) -> String {
    format!("/blob/{hash}")
}

/// What an immutable, content-addressed response says about itself.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Every blob is one of the three KiCad text files the upload keeps.
const TEXT: &str = "text/plain; charset=utf-8";

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "no such blob\n").into_response()
}

/// Whether the request takes gzip, which every browser does.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let coding = coding.trim();
            let name = coding.split(';').next().unwrap_or(coding).trim();
            (name == "gzip" || name == "*") && !coding.contains("q=0")
        })
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
        return Ok(not_found());
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
        (CACHE_CONTROL, IMMUTABLE),
        (VARY, "Accept-Encoding"),
    ];
    Ok(Some(if accepts_gzip(headers) {
        (common, [(CONTENT_ENCODING, "gzip")], compressed).into_response()
    } else {
        (common, bytes).into_response()
    }))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn headers(accept_encoding: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = accept_encoding {
            headers.insert(ACCEPT_ENCODING, value.parse().unwrap());
        }
        headers
    }

    #[test_case(Some("gzip, deflate, br"), true; "a browser")]
    #[test_case(Some("gzip;q=1.0, identity; q=0.5"), true; "weighted")]
    #[test_case(Some("*"), true; "anything")]
    #[test_case(Some("gzip;q=0"), false; "gzip refused")]
    #[test_case(Some("identity"), false; "identity only")]
    #[test_case(None, false; "no header, as curl sends")]
    fn gzip_is_sent_only_to_a_client_that_takes_it(accept: Option<&str>, gzip: bool) {
        assert_eq!(accepts_gzip(&headers(accept)), gzip);
    }

    #[test]
    fn a_blob_url_is_its_hash_under_blob() {
        let hash = BlobHash::of(b"kiewer");
        assert_eq!(
            url(&hash),
            "/blob/ad8e964fa89208513d804735d67ba5e70ac9154e3658b8ffd9eea31853860ad2"
        );
    }
}
