//! A bare HTTP/1.0 client for tests: one request, the status, headers and
//! body back, and a [`Browser`] that keeps cookies between requests. No
//! client crate, so the tests exercise the server's listener the way a
//! browser's first request does.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use kiewer_lib::auth::Auth;
use kiewer_lib::blob::BlobStore;
use kiewer_lib::cache::{NoPurge, Purge};
use kiewer_lib::thumbnail::{NoTasks, Thumbnails};
use kiewer_lib::web::{AppState, serve};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The server's state for a test: `db`, `blobs`, no sign-in provider
/// (a public URL nothing listens on) and no task queue, so a thumbnail
/// past its budget is logged and left. The one place a new part of
/// `AppState` is filled for every test; a test that needs a part of its
/// own calls `AppState::new`.
///
/// # Panics
///
/// Never: an `Auth` with no apps always builds.
#[must_use]
pub fn state(db: PgPool, blobs: Arc<dyn BlobStore>) -> AppState {
    state_with_auth(
        db,
        blobs,
        Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth"),
    )
}

/// The same with `auth`, for a test that registers a provider app.
#[must_use]
pub fn state_with_auth(db: PgPool, blobs: Arc<dyn BlobStore>, auth: Auth) -> AppState {
    state_with(db, blobs, auth, Arc::new(NoPurge))
}

/// The same with `purge`, for a test that reads back what a write
/// purged (`kiewer_lib::cache::Recorder`).
///
/// # Panics
///
/// As [`state`].
#[must_use]
pub fn state_with_purge(db: PgPool, blobs: Arc<dyn BlobStore>, purge: Arc<dyn Purge>) -> AppState {
    state_with(
        db,
        blobs,
        Auth::new(Vec::new(), "http://127.0.0.1:1").expect("an auth"),
        purge,
    )
}

fn state_with(
    db: PgPool,
    blobs: Arc<dyn BlobStore>,
    auth: Auth,
    purge: Arc<dyn Purge>,
) -> AppState {
    AppState::new(
        db,
        blobs,
        Arc::new(auth),
        Arc::new(Thumbnails::new(Arc::new(NoTasks))),
        purge,
    )
}

/// A loopback listener on a port the OS picks, and its address: for a
/// test that needs the address before it builds the state (the public
/// URL a sign-in callback is under).
///
/// # Panics
///
/// When the loopback port cannot be bound.
pub async fn bind_free_port() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

/// The server over `state` on `listener`, running until the test's
/// runtime ends.
pub fn serve_listener(listener: TcpListener, state: AppState) {
    tokio::spawn(serve(listener, state, std::future::pending()));
}

/// The server over `state` on 127.0.0.1 at a port the OS picks, running
/// until the test's runtime ends; its address.
///
/// # Panics
///
/// When the loopback port cannot be bound.
pub async fn serve_on_free_port(state: AppState) -> SocketAddr {
    let (listener, addr) = bind_free_port().await;
    serve_listener(listener, state);
    addr
}

/// What came back: the status, the headers in order with their names
/// lowercased, and the body.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// The first header named `name` (any case), or `None`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every header named `name` (any case), in order.
    #[must_use]
    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// One HTTP/1.0 request at `addr`: `method path`, the extra `headers` as
/// given, and `body` with its length (and a form content type when it is
/// not empty).
///
/// # Panics
///
/// When the server cannot be reached or answers something that is not
/// HTTP/1.x: in a test, that is the failure.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let content_type = (!body.is_empty()).then_some("application/x-www-form-urlencoded");
    send(addr, method, path, headers, content_type, body.as_bytes()).await
}

/// What came back, with the body as bytes: for a response that is not
/// text (a compressed asset, a wasm).
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// The first header named `name` (any case), or `None`.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// One HTTP/1.0 request at `addr` with any body: `method path`, the
/// extra `headers`, `Content-Type: content_type` when given, and `body`
/// with its length.
///
/// # Panics
///
/// As [`request`].
pub async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: &[u8],
) -> Response {
    let raw = send_raw(addr, method, path, headers, content_type, body).await;
    Response {
        status: raw.status,
        headers: raw.headers,
        body: String::from_utf8_lossy(&raw.body).into_owned(),
    }
}

/// As [`send`], with the response body as the bytes that came back.
///
/// # Panics
///
/// As [`request`].
pub async fn send_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: &[u8],
) -> RawResponse {
    use std::fmt::Write as _;
    let mut text = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\n");
    // Writing to a String cannot fail.
    for (name, value) in headers {
        let _ = write!(text, "{name}: {value}\r\n");
    }
    if let Some(content_type) = content_type {
        let _ = write!(text, "Content-Type: {content_type}\r\n");
    }
    let _ = write!(text, "Content-Length: {}\r\n\r\n", body.len());
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(text.as_bytes())
        .await
        .expect("write the request head");
    stream
        .write_all(body)
        .await
        .expect("write the request body");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read the response");
    let blank = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a header block ended by a blank line");
    let head = String::from_utf8_lossy(&raw[..blank]).into_owned();
    let body = raw[blank + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    RawResponse {
        status,
        headers,
        body,
    }
}

/// `GET path` at `addr`: the status code and the response body.
///
/// # Panics
///
/// As [`request`].
pub async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let response = request(addr, "GET", path, &[], "").await;
    (response.status, response.body)
}

/// One part of a multipart body: a text field without a file name, a
/// file with one.
#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    pub name: &'a str,
    pub file_name: Option<&'a str>,
    pub body: &'a [u8],
}

impl<'a> Part<'a> {
    /// A text field.
    #[must_use]
    pub fn text(name: &'a str, value: &'a str) -> Self {
        Self {
            name,
            file_name: None,
            body: value.as_bytes(),
        }
    }

    /// A file under the field `files`, as the upload form names it.
    #[must_use]
    pub fn file(file_name: &'a str, body: &'a [u8]) -> Self {
        Self {
            name: "files",
            file_name: Some(file_name),
            body,
        }
    }
}

/// A client that keeps the cookies one server sets, as a browser does:
/// a `Set-Cookie` stores or, with `Max-Age=0`, forgets; every request
/// carries what is stored. Paths and expiry are not modelled beyond
/// that; a test asserts the attributes it cares about on the response.
#[derive(Debug)]
pub struct Browser {
    addr: SocketAddr,
    cookies: BTreeMap<String, String>,
}

impl Browser {
    /// A browser with no cookies, pointed at `addr`.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            cookies: BTreeMap::new(),
        }
    }

    /// The server's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The stored value of the cookie `name`.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    /// Store a cookie as if the server had set it: a session a test made
    /// in the database goes in a browser this way.
    pub fn set_cookie(&mut self, name: &str, value: &str) {
        self.cookies.insert(name.to_owned(), value.to_owned());
    }

    /// `GET path` with the stored cookies.
    pub async fn get(&mut self, path: &str) -> Response {
        self.send("GET", path, None, b"").await
    }

    /// `POST path` with `body` as a form and the stored cookies.
    pub async fn post_form(&mut self, path: &str, body: &str) -> Response {
        let content_type = (!body.is_empty()).then_some("application/x-www-form-urlencoded");
        self.send("POST", path, content_type, body.as_bytes()).await
    }

    /// `POST path` as a `multipart/form-data` body of `parts`, with the
    /// stored cookies: what a browser sends for a form with files.
    pub async fn post_multipart(&mut self, path: &str, parts: &[Part<'_>]) -> Response {
        let boundary = "kiewer-test-boundary-7d3f";
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match part.file_name {
                Some(file_name) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{file_name}\"\r\n\
                         Content-Type: application/octet-stream\r\n\r\n",
                        part.name
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
                        part.name
                    )
                    .as_bytes(),
                ),
            }
            body.extend_from_slice(part.body);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let content_type = format!("multipart/form-data; boundary={boundary}");
        self.send("POST", path, Some(&content_type), &body).await
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Response {
        let cookie_header = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let headers: Vec<(&str, &str)> = if cookie_header.is_empty() {
            Vec::new()
        } else {
            vec![("Cookie", cookie_header.as_str())]
        };
        let response = send(self.addr, method, path, &headers, content_type, body).await;
        for set_cookie in response.headers_named("set-cookie") {
            let mut parts = set_cookie.split(';').map(str::trim);
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
                continue;
            };
            let forgets = parts.any(|attr| attr.eq_ignore_ascii_case("Max-Age=0"));
            if forgets || value.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        response
    }
}
