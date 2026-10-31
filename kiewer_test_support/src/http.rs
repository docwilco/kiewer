//! A bare HTTP/1.0 client for tests: one request, the status, headers and
//! body back, and a [`Browser`] that keeps cookies between requests. No
//! client crate, so the tests exercise the server's listener the way a
//! browser's first request does.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use kiewer_lib::web::{AppState, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    use std::fmt::Write as _;
    let mut text = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\n");
    // Writing to a String cannot fail.
    for (name, value) in headers {
        let _ = write!(text, "{name}: {value}\r\n");
    }
    if !body.is_empty() {
        text.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
    }
    let _ = write!(text, "Content-Length: {}\r\n\r\n{body}", body.len());
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(text.as_bytes())
        .await
        .expect("write the request");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .expect("read the response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("a header block ended by a blank line");
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
    Response {
        status,
        headers,
        body: body.to_owned(),
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

    /// `GET path` with the stored cookies.
    pub async fn get(&mut self, path: &str) -> Response {
        self.send("GET", path, "").await
    }

    /// `POST path` with `body` as a form and the stored cookies.
    pub async fn post_form(&mut self, path: &str, body: &str) -> Response {
        self.send("POST", path, body).await
    }

    async fn send(&mut self, method: &str, path: &str, body: &str) -> Response {
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
        let response = request(self.addr, method, path, &headers, body).await;
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
