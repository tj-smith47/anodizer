//! Shared in-process HTTP responder for unit tests across the workspace.
//!
//! **All test HTTP responders MUST use this helper.** Inline duplicates
//! have a known race; see commit `45a8e78` (chocolatey CI flake) and the
//! workspace-wide centralization that followed it.
//!
//! ## History
//!
//! Originally each consumer crate had a near-identical inline
//! `spawn_oneshot_http_responder` (≈11 copies across `stage-publish`,
//! `stage-release`, `stage-changelog`, `cli`, and `core`). All of them
//! shared the same race: read one 8–16 KiB chunk (or time out at 500 ms),
//! write the canned response, and close the connection — without first
//! consuming the client's full HTTP request body.
//!
//! On slow CI runners (notably `ubuntu-latest` and `macos-latest` under
//! load) the responder could close the socket while the client was still
//! uploading its multipart request body. The client then saw
//! `BrokenPipe` / `Connection reset` on its next write, interpreted that
//! as a transport-layer failure (NOT an HTTP 503/401), and the
//! surrounding retry loop exhausted itself on the drain-phase 503s. That
//! manifested as intermittent test failures with `counter == 4` and an
//! "attempt 4" error message — observed in tests such as
//! `chocolatey::package::tests::push_nupkg_*` and
//! `github::secondary_rate_limit::*`.
//!
//! ## The fix
//!
//! Read the full HTTP request (request line + headers up to the
//! `\r\n\r\n` terminator, then exactly `Content-Length` bytes of body)
//! before writing the response. If `Content-Length` is missing or
//! unparseable, fall back to a best-effort drain bounded by a generous
//! deadline. Per-connection read timeout is also bumped from 500 ms to
//! 5 s since 500 ms is too tight for scheduling jitter on shared CI
//! runners.
//!
//! ## Feature gating
//!
//! This module is part of `crate::test_helpers`, which is gated behind
//! the `test-helpers` Cargo feature. Sibling crates pull it in as:
//!
//! ```toml
//! [dev-dependencies]
//! anodizer-core = { workspace = true, features = ["test-helpers"] }
//! ```
//!
//! and call it from `#[cfg(test)]` modules:
//!
//! ```rust,ignore
//! use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
//!
//! let (addr, calls) = spawn_oneshot_http_responder(vec![
//!     "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
//!     "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
//! ]);
//! ```

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Per-connection read timeout. 5 seconds is wildly generous for a
/// localhost loopback but tolerates CI scheduling jitter (the previous
/// 500 ms timeout occasionally fired on cold-started `ubuntu-latest` and
/// `macos-latest` runners). The test still completes in a few ms on the
/// happy path because we break out as soon as we've read the full
/// request.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on how long a single request read may take, in case a
/// pathological client never sends `\r\n\r\n`. Same generous bound as
/// `READ_TIMEOUT`.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Bind an ephemeral-port TCP listener and serve `responses` in order,
/// one per accepted connection, then enter a brief drain phase that
/// soaks up any in-flight retries the client may have initiated before
/// its loop noticed it had a success.
///
/// Returns the bound address and a counter that increments **only for
/// the canned `responses`** — drain-phase straggler connections are
/// served but NOT counted, so tests can assert
/// `counter.load() == <expected attempts>` without false positives from
/// over-eager client middleware (e.g. octocrab's tower retry layer
/// connecting more times than the user-level policy permits).
///
/// To serve the same response N times (e.g. when a retrying client is
/// expected to make N attempts), pass `vec![resp; n]`.
pub fn spawn_oneshot_http_responder(responses: Vec<&'static str>) -> (SocketAddr, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let counter = Arc::new(AtomicU32::new(0));
    spawn_serve_thread(listener, counter.clone(), responses);
    (addr, counter)
}

/// Like [`spawn_oneshot_http_responder`], but the response queue is built
/// AFTER the listener binds, via `make_responses(addr)`. Use this when a
/// canned response must embed the responder's own URL — e.g. a Cloudsmith
/// `files/create` body whose `upload_url` points the follow-up presigned
/// upload back at this same responder.
///
/// Owned `String` responses (rather than `&'static str`) so the closure can
/// `format!` the bound address in. Same one-per-connection serving + drain
/// semantics and the same call counter (canned responses only).
pub fn spawn_oneshot_http_responder_with<F>(make_responses: F) -> (SocketAddr, Arc<AtomicU32>)
where
    F: FnOnce(SocketAddr) -> Vec<String>,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let responses = make_responses(addr);
    let counter = Arc::new(AtomicU32::new(0));
    spawn_serve_thread(listener, counter.clone(), responses);
    (addr, counter)
}

/// Serve `responses` one-per-connection (incrementing `counter` per canned
/// response), then enter a drain phase. Generic over `AsRef<str>` so both
/// the `&'static str` and owned-`String` spawners delegate here.
///
/// Drain phase — soak up any in-flight connect attempts that the client may
/// have initiated before its retry returned success. Without this, a stray
/// SYN arriving after the listener is dropped sees `Connection refused (os
/// error 111)` on Linux and the test goes flaky on slow CI runners. We keep
/// accepting briefly and serve any straggler an empty 503; the client logic
/// (which has already returned success) ignores it. Drain-phase connections
/// are NOT counted: tests pin `counter.load() == <canned attempts>` and an
/// over-eager client middleware (e.g. octocrab's tower retry layer making
/// extra connects beyond the user-level retry policy) must not inflate that
/// assertion.
fn spawn_serve_thread<R: AsRef<str> + Send + 'static>(
    listener: TcpListener,
    counter: Arc<AtomicU32>,
    responses: Vec<R>,
) {
    std::thread::spawn(move || {
        for resp in responses.iter() {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            counter.fetch_add(1, Ordering::SeqCst);
            serve_one(stream, resp.as_ref());
        }
        let _ = listener.set_nonblocking(true);
        let drain_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < drain_deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    serve_one(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                    );
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
}

/// Capture the first request bytes and reply with a canned response so a
/// caller can assert specific headers were sent verbatim. Serves
/// **exactly one** connection (no retry/drain phase) — pair with a test
/// that issues a single HTTP request.
///
/// Returns the bound address and a `Mutex<String>` that holds the raw
/// request bytes (request line + headers + as much of the body as fit
/// in the first read chunk after the headers — sufficient for header
/// assertions, which is the only documented use).
///
/// Like [`spawn_oneshot_http_responder`], this consumes the full HTTP
/// request before writing the response so the client never sees
/// `BrokenPipe` on its body write.
pub fn spawn_request_capturing_responder(
    response: &'static str,
) -> (SocketAddr, Arc<std::sync::Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let captured_inner = captured.clone();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
            let request_bytes = consume_request_capturing(&mut stream);
            *captured_inner.lock().unwrap() = String::from_utf8_lossy(&request_bytes).to_string();
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    (addr, captured)
}

/// Consume one HTTP request from `stream` (headers + Content-Length-
/// bound body), then write `resp`, flush, and shut down both halves of
/// the connection.
///
/// The full-request read is the fix for the v0.3.0 CI flake: closing
/// the socket before the client has finished uploading its request body
/// raced the client's send buffer and produced `BrokenPipe` on the
/// client side, which was then mis-classified as a transport-layer
/// failure and triggered a spurious retry.
fn serve_one(mut stream: TcpStream, resp: &str) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    consume_request(&mut stream);
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Read the full HTTP request from `stream`: headers up to the first
/// `\r\n\r\n`, then exactly `Content-Length` bytes of body if that
/// header is present. Best-effort and fully fault-tolerant — any I/O
/// error or timeout simply ends the read; we never propagate.
fn consume_request(stream: &mut TcpStream) {
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut accum: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    // Read until we've seen \r\n\r\n (end of headers) or hit
    // the deadline / EOF / I/O error.
    let header_end = loop {
        if Instant::now() >= deadline {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return, // EOF before headers complete — give up.
            Ok(n) => {
                accum.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_double_crlf(&accum) {
                    break pos + 4;
                }
                // Guard against unbounded growth from a malformed client.
                if accum.len() > 1 << 20 {
                    return;
                }
            }
            Err(_) => return,
        }
    };

    // Parse Content-Length and drain that many bytes of body.
    let content_length = parse_content_length(&accum[..header_end]);
    let already_have = accum.len() - header_end;
    let Some(total_body) = content_length else {
        // No Content-Length — most non-body requests (GET, HEAD) and
        // some streaming clients fall here. We've already read at least
        // the headers, which is sufficient for the responder to write a
        // canned reply. Don't block further.
        return;
    };

    if already_have >= total_body {
        return;
    }
    let mut remaining = total_body - already_have;
    while remaining > 0 {
        if Instant::now() >= deadline {
            return;
        }
        let want = remaining.min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return, // EOF early — give up gracefully.
            Ok(n) => {
                remaining -= n;
            }
            Err(_) => return,
        }
    }
}

/// Variant of [`consume_request`] that returns the bytes it consumed.
/// Used by [`spawn_request_capturing_responder`] so callers can assert
/// on the raw request that was sent.
fn consume_request_capturing(stream: &mut TcpStream) -> Vec<u8> {
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut accum: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    let header_end = loop {
        if Instant::now() >= deadline {
            return accum;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return accum,
            Ok(n) => {
                accum.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_double_crlf(&accum) {
                    break pos + 4;
                }
                if accum.len() > 1 << 20 {
                    return accum;
                }
            }
            Err(_) => return accum,
        }
    };

    let content_length = parse_content_length(&accum[..header_end]);
    let already_have = accum.len() - header_end;
    let Some(total_body) = content_length else {
        return accum;
    };

    if already_have >= total_body {
        return accum;
    }
    let mut remaining = total_body - already_have;
    while remaining > 0 {
        if Instant::now() >= deadline {
            return accum;
        }
        let want = remaining.min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return accum,
            Ok(n) => {
                accum.extend_from_slice(&chunk[..n]);
                remaining -= n;
            }
            Err(_) => return accum,
        }
    }
    accum
}

/// Find the byte offset of the first `\r\n\r\n` in `buf`, returning the
/// index of the first byte of that sequence. Naive scan is fine — the
/// inputs are tiny HTTP headers.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the `Content-Length` header value from a raw HTTP header
/// block (everything up to and including the terminating `\r\n\r\n`).
/// Case-insensitive on the header name; the value is trimmed and
/// parsed as a `usize`. Returns `None` if absent or unparseable.
fn parse_content_length(header_block: &[u8]) -> Option<usize> {
    // Header block is ASCII per RFC 7230 (header field names are
    // tokens; values are visible ASCII + obs-text). Lossy decode is
    // safe for the parse — any non-ASCII byte would not match the
    // ASCII-lowercased prefix anyway.
    let as_str = std::str::from_utf8(header_block).ok()?;
    for line in as_str.split("\r\n") {
        // Skip the request line (no colon), blank lines, and any
        // malformed header line. `continue` rather than `?` so a
        // colon-less line doesn't short-circuit the whole scan.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn find_double_crlf_locates_header_terminator() {
        // "GET / HTTP/1.1" = 14 bytes, "\r\n" = 16, "Host: x" = 23,
        // then "\r\n\r\n" starts at offset 23.
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        assert_eq!(find_double_crlf(buf), Some(23));
    }

    #[test]
    fn find_double_crlf_returns_none_when_absent() {
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
    }

    #[test]
    fn parse_content_length_case_insensitive() {
        let hdr = b"PUT / HTTP/1.1\r\nHost: x\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(hdr), Some(42));
        let hdr = b"PUT / HTTP/1.1\r\nHost: x\r\ncontent-length: 7\r\n\r\n";
        assert_eq!(parse_content_length(hdr), Some(7));
    }

    #[test]
    fn parse_content_length_missing_returns_none() {
        let hdr = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_content_length(hdr), None);
    }

    #[test]
    fn parse_content_length_unparseable_returns_none() {
        let hdr = b"PUT / HTTP/1.1\r\nContent-Length: chunked\r\n\r\n";
        assert_eq!(parse_content_length(hdr), None);
    }

    /// End-to-end: spin up the responder, send a multipart-ish PUT with
    /// a body that exceeds the initial read chunk, and verify the
    /// canned response comes back intact. Regression test for the
    /// chocolatey CI flake — pre-fix this would race the client's body
    /// send.
    #[test]
    fn responder_consumes_full_body_before_responding() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let canned =
            "HTTP/1.1 201 Created\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nok";
        let (addr, calls) = spawn_oneshot_http_responder(vec![canned]);

        // Body large enough to exceed our 8 KiB read chunk so the
        // responder MUST do >1 read to consume it.
        let body = vec![b'x'; 32 * 1024];
        let body_len = body.len();
        let request = format!(
            "PUT /api/v2/package HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {body_len}\r\n\r\n"
        );

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("write headers");
        stream.write_all(&body).expect("write body");
        stream.flush().expect("flush");

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read full response");
        assert!(
            response.starts_with("HTTP/1.1 201 Created"),
            "unexpected response: {response:?}"
        );
        assert!(
            response.ends_with("ok"),
            "unexpected response: {response:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one accept");
    }

    /// Capturing variant must record the full request line and headers
    /// so a caller can assert e.g. an Authorization header was sent.
    #[test]
    fn capturing_responder_records_request_headers() {
        use std::io::Write;
        use std::net::TcpStream;

        let canned = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let (addr, captured) = spawn_request_capturing_responder(canned);

        let request = "GET /search/issues HTTP/1.1\r\n\
                       Host: 127.0.0.1\r\n\
                       Authorization: Bearer secret-token\r\n\
                       Content-Length: 0\r\n\r\n";
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().expect("flush");
        // Give the responder thread a beat to capture+write before we
        // inspect the captured buffer.
        std::thread::sleep(Duration::from_millis(50));
        let _ = stream.shutdown(std::net::Shutdown::Both);

        // Poll briefly for the capture (the responder writes the
        // captured string from a worker thread).
        let deadline = Instant::now() + Duration::from_secs(2);
        let captured_str = loop {
            let s = captured.lock().unwrap().clone();
            if !s.is_empty() || Instant::now() >= deadline {
                break s;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let lower = captured_str.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer secret-token"),
            "captured request missing Authorization: {captured_str:?}"
        );
    }
}
