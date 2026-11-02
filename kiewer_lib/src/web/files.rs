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
//! Neither answer is kept for long ([`Policy::NotFound`]), since a
//! takedown can be lifted.

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_DISPOSITION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use super::{AppState, blob};
use crate::account::Handle;
use crate::blob::object_name;
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
/// path is percent-encoded, its slashes kept.
#[must_use]
pub fn url(handle: &Handle, slug: &Slug, hash: RevisionHash, path: &TreePath) -> String {
    format!(
        "/{handle}/{slug}/r/{hash}/f/{}",
        utf8_percent_encode(path.as_str(), IN_PATH)
    )
}

/// The `Content-Disposition` that saves the file as `name`: an
/// attachment with the name as a quoted string (RFC 6266), `"` and `\`
/// escaped. A character outside printable ASCII cannot go in a quoted
/// string, and a `%` must not (some clients decode a `%XX` there, RFC
/// 6266 Appendix D), so a name holding either goes as the `filename*`
/// of RFC 8187 too, UTF-8 percent-encoded, with those characters
/// replaced by `_` in the `filename` for a client that reads only that.
fn content_disposition(name: &str) -> String {
    let portable = |c: char| matches!(c, ' '..='~') && c != '%';
    let plain: String = name
        .chars()
        .map(|c| match c {
            '"' | '\\' => format!("\\{c}"),
            c if portable(c) => c.to_string(),
            _ => "_".to_owned(),
        })
        .collect();
    let mut value = format!("attachment; filename=\"{plain}\"");
    if !name.chars().all(portable) {
        value.push_str("; filename*=UTF-8''");
        value.push_str(&utf8_percent_encode(name, IN_EXT_VALUE).to_string());
    }
    value
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [Policy::NotFound.header()],
        "no such file\n",
    )
        .into_response()
}

/// `GET /{handle}/{slug}/r/{hash}/f/{path}`: the file at `path` in the
/// revision, as an attachment named after it. A URL that does not decode
/// to UTF-8 names nothing a tree holds, so it is a 404 like any other,
/// and a handle or slug that is not one stops before any lookup.
///
/// # Errors
///
/// The database's, the store's, [`Error::ReadBlob`] when the tree names
/// a blob the store does not hold (the tree is the record, so that is
/// lost data, not a missing file), or [`Error::CorruptBlob`] when the
/// stored bytes do not hash to their name: a 500 with the cause in the
/// log.
async fn serve(
    State(state): State<AppState>,
    params: Result<Path<(String, String, String, String)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response> {
    let Ok(Path((handle, slug, hash, path))) = params else {
        return Ok(not_found());
    };
    let (Ok(handle), Ok(slug), Ok(hash), Ok(path)) = (
        handle.parse::<Handle>(),
        slug.parse::<Slug>(),
        hash.parse::<RevisionHash>(),
        path.parse::<TreePath>(),
    ) else {
        return Ok(not_found());
    };
    let Some((design, _owner)) = design::find(&state.db, handle.as_str(), slug.as_str()).await?
    else {
        return Ok(not_found());
    };
    let Some(entry) = design::file_at(&state.db, design.id, hash, &path).await? else {
        return Ok(not_found());
    };
    if design::is_blocked(&state.db, &entry.blob).await? {
        return Ok(blob::taken_down());
    }
    let Some(mut response) = blob::stored(&state, entry.blob, &headers).await? else {
        return Err(Error::ReadBlob {
            hash: entry.blob,
            path: object_name(&entry.blob).into(),
            source: std::io::ErrorKind::NotFound.into(),
        });
    };
    // The name is a path segment, so it holds no control character and
    // the disposition is printable ASCII: a valid header value.
    let disposition = HeaderValue::from_str(&content_disposition(path.file_name()))
        .map_err(|error| Error::Malformed(format!("disposition for {path}: {error}")))?;
    response
        .headers_mut()
        .insert(CONTENT_DISPOSITION, disposition);
    Ok(response)
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
    #[test_case("50% done.kicad_pcb", r#"attachment; filename="50_ done.kicad_pcb"; filename*=UTF-8''50%25%20done.kicad_pcb"#; "a percent sign goes in the filename star")]
    #[test_case("a%20b.kicad_pcb", r#"attachment; filename="a_20b.kicad_pcb"; filename*=UTF-8''a%2520b.kicad_pcb"#; "text a client would decode stays literal")]
    #[test_case("ü%.kicad_pcb", r#"attachment; filename="__.kicad_pcb"; filename*=UTF-8''%C3%BC%25.kicad_pcb"#; "both at once")]
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
    fn the_url_escapes_the_path_and_keeps_its_folders(path: &str, encoded: &str) {
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
