//! One GET, to our own machine, written by hand.
//!
//! The shell asks the servers two questions — is a show open, and is it running —
//! and that is the whole of its need for HTTP. A client library would bring TLS, a
//! connection pool, an async runtime and a redirect policy to fetch a dozen bytes
//! from a socket on this machine, so this asks in the plainest way there is.
//!
//! HTTP/1.0 with no keep-alive, so the reply ends when the server closes the
//! connection and there is no length to parse. Short timeouts throughout: the answer
//! is either immediate or it is not coming, and the caller is a person waiting to
//! find out whether they may quit.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Is anything answering on this port at all?
///
/// Separate from [`get_json`] because "refused" and "answered something I could not
/// read" mean opposite things to a caller deciding whether to interrupt a show.
pub fn is_listening(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// `GET path` from `127.0.0.1:port`, parsed as JSON. `None` for anything that is not
/// a prompt answer — refused, slow, not JSON, or an error status.
pub fn get_json(port: u16, path: &str) -> Option<serde_json::Value> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    sock.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
    sock.set_write_timeout(Some(Duration::from_millis(500))).ok()?;
    write!(
        sock,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    sock.flush().ok()?;

    let mut raw = Vec::new();
    // Bounded, because a reply this size that keeps arriving is a server answering a
    // different question. Reads stop at the cap rather than filling memory.
    sock.take(64 * 1024).read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").or_else(|| text.split_once("\n\n"))?;
    if !head.lines().next()?.contains(" 200") {
        return None;
    }
    serde_json::from_str(body.trim()).ok()
}
