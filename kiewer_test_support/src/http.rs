//! A bare HTTP/1.0 client for tests: one request, the status and the body
//! back. No client crate, so the tests exercise the server's listener the
//! way a browser's first request does.

use std::net::SocketAddr;

use kiewer_lib::web::{AppState, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The server over `state` on 127.0.0.1 at a port the OS picks, running
/// until the test's runtime ends; its address.
///
/// # Panics
///
/// When the loopback port cannot be bound.
pub async fn serve_on_free_port(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(serve(listener, state, std::future::pending()));
    addr
}

/// `GET path` at `addr`: the status code and the response body.
///
/// # Panics
///
/// When the server cannot be reached or answers something that is not
/// HTTP/1.x: in a test, that is the failure.
pub async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes())
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
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, body.to_owned())
}
