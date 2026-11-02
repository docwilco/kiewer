//! Per-file downloads (docs/v1-scope.md, Pages and URLs):
//! `/{handle}/{slug}/r/{hash}/f/{path}` is one file of a revision under
//! its original name, as an attachment. The bytes are the blob's, served
//! as `/blob/{hash}` serves them (the stored gzip to a client that takes
//! it, decoded for one that does not, checked against the hash), under a
//! URL that names the revision and the path, so the browser saves the
//! file as it was uploaded and a link to it survives a push. A revision
//! is immutable, so the response is cacheable for a year like a blob.
//! The lookup is scoped by the design, as every read of a revision is
//! (ADR 0002): a revision the design does not reach is 404, as is a path
//! its tree does not hold or text that is not a path. A blob the
//! operator has blocked is 410: the file existed and was taken down.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_TYPE, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use super::{AppState, blob};
use crate::account::Handle;
use crate::blob::{blocking, gunzip, verified};
use crate::cache::Policy;
use crate::design::{self, Slug};
use crate::error::{Error, Result};
use crate::revision::{RevisionHash, TreePath};

/// The file routes, merged into the server's router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/{handle}/{slug}/r/{hash}/f/{*path}", get(serve))
}

/// The characters a path segment in a URL must escape: everything a
/// browser would not send as it is (controls, space, the delimiters) and
/// the two that would end or split the path, `?` and `#`. `/` stays: the
/// path's folders are the URL's segments.
const IN_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// The characters RFC 8187's `ext-value` must escape in a `filename*`:
/// everything but its `attr-char` set: the letters, the digits, the
/// backtick and `!#$&+-.^_|~`.
const IN_EXT_VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'%')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'{')
    .add(b'}');

/// The URL `path` of revision `hash` of `/{handle}/{slug}` downloads
/// at. The handle, the slug and the hash are their own characters; the
/// path is percent-encoded segment by segment.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug, hash: RevisionHash, path: &TreePath) -> String {
    format!(
        "/{handle}/{slug}/r/{hash}/f/{}",
        utf8_percent_encode(path.as_str(), IN_PATH)
    )
}

/// The `Content-Disposition` that saves the file as `name`: an
/// attachment with the name as a quoted string (RFC 6266), `"` and `\`
/// escaped. A name with a character outside printable ASCII cannot go
/// in a quoted string, so it goes as the `filename*` of RFC 8187, UTF-8
/// percent-encoded, with those characters replaced by `_` in the plain
/// `filename` for a client that reads only that.
#[must_use]
pub fn content_disposition(name: &str) -> String {
    let plain: String = name
        .chars()
        .map(|c| match c {
            '"' | '\\' => format!("\\{c}"),
            ' '..='~' => c.to_string(),
            _ => "_".to_owned(),
        })
        .collect();
    let mut value = format!("attachment; filename=\"{plain}\"");
    if !name.is_ascii() || name.chars().any(char::is_control) {
        value.push_str("; filename*=UTF-8''");
        value.push_str(&utf8_percent_encode(name, IN_EXT_VALUE).to_string());
    }
    value
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "no such file\n").into_response()
}

/// `GET /{handle}/{slug}/r/{hash}/f/{path}`: the file at `path` in the
/// revision, as an attachment named after it.
///
/// # Errors
///
/// The database's, the store's, or [`Error::CorruptBlob`] when the
/// stored bytes do not hash to their name: a 500 with the cause in the
/// log.
async fn serve(
    State(state): State<AppState>,
    Path((handle, slug, hash, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let (Ok(hash), Ok(path)) = (hash.parse::<RevisionHash>(), path.parse::<TreePath>()) else {
        return Ok(not_found());
    };
    let Some((design, _owner)) = design::find(&state.db, &handle, &slug).await? else {
        return Ok(not_found());
    };
    let Some(entry) = design::file_at(&state.db, design.id, hash, &path).await? else {
        return Ok(not_found());
    };
    if design::is_blocked(&state.db, &entry.blob).await? {
        return Ok((StatusCode::GONE, "this file was taken down\n").into_response());
    }
    let blob = entry.blob;
    let Some(compressed) = state.blobs.get_gzip(&blob).await? else {
        // The tree names a blob the store lost: the tree is the record,
        // so this is the store's fault, not a missing file.
        return Err(Error::Malformed(format!(
            "blob {blob} of {path} in revision {hash} is not in the store"
        )));
    };
    // Decoding and hashing are CPU work, off the runtime's threads. The
    // decoded bytes are the body for a client without gzip.
    let (compressed, bytes) = blocking(move || {
        let bytes = gunzip(&compressed).map_err(|source| Error::ReadBlob {
            hash: blob,
            path: crate::blob::object_name(&blob).into(),
            source,
        })?;
        let bytes = verified(&blob, bytes)?;
        Ok((compressed, bytes))
    })
    .await?;
    // The name is a path segment, so it holds no control character and
    // the disposition is printable ASCII: a valid header value.
    let disposition = HeaderValue::from_str(&content_disposition(path.file_name()))
        .map_err(|error| Error::Malformed(format!("disposition for {path}: {error}")))?;
    let common = [
        (CONTENT_TYPE, HeaderValue::from_static(blob::TEXT)),
        (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        (
            CACHE_CONTROL,
            HeaderValue::from_static(Policy::Immutable.value()),
        ),
        (VARY, HeaderValue::from_static("Accept-Encoding")),
        (CONTENT_DISPOSITION, disposition),
    ];
    Ok(if blob::accepts_gzip(&headers) {
        (common, [(CONTENT_ENCODING, "gzip")], compressed).into_response()
    } else {
        (common, bytes).into_response()
    })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("midi8knob.kicad_pcb", r#"attachment; filename="midi8knob.kicad_pcb""#; "a plain name")]
    #[test_case("power supply.kicad_sch", r#"attachment; filename="power supply.kicad_sch""#; "a space")]
    #[test_case(r#"say "hi".kicad_sch"#, r#"attachment; filename="say \"hi\".kicad_sch""#; "a quote is escaped")]
    #[test_case(r"a\b.kicad_sch", r#"attachment; filename="a\\b.kicad_sch""#; "a backslash is escaped")]
    #[test_case("semi;colon.kicad_pcb", r#"attachment; filename="semi;colon.kicad_pcb""#; "a semicolon is fine quoted")]
    #[test_case("Stromversorgung ü.kicad_sch", r#"attachment; filename="Stromversorgung _.kicad_sch"; filename*=UTF-8''Stromversorgung%20%C3%BC.kicad_sch"#; "a non-ASCII name gets a filename star")]
    #[test_case("日本語.kicad_pcb", r#"attachment; filename="___.kicad_pcb"; filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E.kicad_pcb"#; "a name with no ASCII at all")]
    #[test_case("50% done.kicad_pcb", r#"attachment; filename="50% done.kicad_pcb""#; "a percent sign is fine quoted")]
    #[test_case("ü%.kicad_pcb", r#"attachment; filename="_%.kicad_pcb"; filename*=UTF-8''%C3%BC%25.kicad_pcb"#; "a percent sign is escaped in the filename star")]
    fn the_disposition_names_the_file_as_rfc_6266_and_8187_spell_it(name: &str, expected: &str) {
        assert_eq!(content_disposition(name), expected);
        assert!(
            HeaderValue::from_str(&content_disposition(name)).is_ok(),
            "a header value"
        );
    }

    #[test_case("midi8knob.kicad_pcb", "midi8knob.kicad_pcb"; "a plain name")]
    #[test_case("sub/dir/power supply.kicad_sch", "sub/dir/power%20supply.kicad_sch"; "folders stay segments, a space is escaped")]
    #[test_case("50%.kicad_pcb", "50%25.kicad_pcb"; "a percent sign")]
    #[test_case("what?.kicad_pcb#1", "what%3F.kicad_pcb%231"; "a query or fragment delimiter")]
    #[test_case("ü.kicad_sch", "%C3%BC.kicad_sch"; "non-ASCII as UTF-8")]
    #[test_case("a+b&c=d.kicad_pcb", "a+b&c=d.kicad_pcb"; "sub-delimiters a path may hold")]
    fn the_url_escapes_the_path_segment_by_segment(path: &str, encoded: &str) {
        let slug: Slug = "demo".parse().unwrap();
        let handle: Handle = "alice".parse().unwrap();
        let hash: RevisionHash = "6da0f20b5d53135dbecca52d60fb77ba50a2cf637be3be968ed20712e8c9ea85"
            .parse()
            .unwrap();
        let path: TreePath = path.parse().unwrap();
        assert_eq!(
            url(&handle, &slug, hash, &path),
            format!("/alice/demo/r/{hash}/f/{encoded}")
        );
    }
}
