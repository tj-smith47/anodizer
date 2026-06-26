//! HTTPS-capable variant of [`crate::test_helpers::responder`].
//!
//! Mirrors the shape of [`crate::test_helpers::responder::spawn_oneshot_http_responder`]
//! but terminates a TLS handshake against a fresh per-call self-signed
//! certificate. Tests that drive `reqwest` against a hard-coded
//! `https://...` URL (notably `crates/stage-release/src/github/rate_limit.rs`,
//! which targets `https://api.github.com/rate_limit`) can point
//! `reqwest` at this responder by overriding the relevant base-URL env
//! var and pairing with [`https_test_client`] to bypass cert validation.
//!
//! ## Conventions inherited from the plain-HTTP responder
//!
//! - Bind ephemeral `127.0.0.1:0`, return the bound `SocketAddr`.
//! - One canned response per accepted connection, served in order.
//! - Full HTTP request (headers + `Content-Length`-bound body) is
//!   consumed before the response is written, killing the
//!   `BrokenPipe`-on-client-write race that flaked older inline
//!   responders. See [`responder`](crate::test_helpers::responder) for
//!   the full incident history.
//! - Drain phase soaks up over-eager retries from middleware (e.g.
//!   `tower` retry layers) so the returned counter pins to exactly the
//!   canned-response count.
//!
//! ## Self-signed cert lifecycle
//!
//! A fresh keypair + leaf cert (valid for `127.0.0.1` and `localhost`)
//! is generated per call via `rcgen::generate_simple_self_signed`. The
//! cert is held only by the spawned acceptor thread — there is no
//! shared trust store to leak across tests.
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::atomic::Ordering;
//! use anodizer_core::test_helpers::https_responder::{
//!     spawn_oneshot_https_responder, https_test_client,
//! };
//!
//! # async fn demo() {
//! let (addr, calls) = spawn_oneshot_https_responder(vec![
//!     "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"remaining\":99}",
//! ]);
//! let client = https_test_client();
//! let url = format!("https://{}/rate_limit", addr);
//! let resp = client.get(&url).send().await.unwrap();
//! assert_eq!(resp.status(), 200);
//! assert_eq!(calls.load(Ordering::SeqCst), 1);
//! # }
//! ```

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// Per-connection read timeout. Matches the plain-HTTP responder's
/// 5-second budget — wildly generous for loopback but tolerates CI
/// scheduling jitter (the previous 500 ms timeout flaked on cold-started
/// shared runners).
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on how long a single request read may take, in case a
/// pathological client never sends `\r\n\r\n`.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Bind an ephemeral-port TCP listener wrapped in a TLS terminator, and
/// serve `responses` in order — one per accepted connection — then
/// enter a brief drain phase that soaks up in-flight retries.
///
/// Returns the bound address and a counter that increments **only for
/// the canned `responses`**. Drain-phase straggler connections are
/// served an empty 503 but NOT counted, matching the plain-HTTP
/// variant's contract.
///
/// The self-signed cert is valid for `127.0.0.1` and `localhost`;
/// pair with [`https_test_client`] (which sets
/// `danger_accept_invalid_certs`) so the client tolerates the
/// untrusted cert chain.
pub fn spawn_oneshot_https_responder(responses: Vec<&'static str>) -> (SocketAddr, Arc<AtomicU32>) {
    crate::tls::install_default_crypto_provider();

    let server_config = Arc::new(build_self_signed_server_config());

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let counter = Arc::new(AtomicU32::new(0));
    let counter_inner = counter.clone();

    std::thread::spawn(move || {
        for resp in responses.iter() {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            counter_inner.fetch_add(1, Ordering::SeqCst);
            serve_one_tls(stream, server_config.clone(), resp);
        }
        // Drain phase — see plain-HTTP responder for incident history.
        let _ = listener.set_nonblocking(true);
        let drain_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < drain_deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    serve_one_tls(
                        stream,
                        server_config.clone(),
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

    (addr, counter)
}

/// Build a `reqwest::Client` that trusts any TLS certificate. Required
/// for talking to [`spawn_oneshot_https_responder`] since its
/// per-call self-signed cert is not in any trust store.
///
/// The client uses `reqwest`'s `rustls-tls` backend (set workspace-wide
/// at `Cargo.toml`'s `reqwest` features list) so the cert-validation
/// override is honoured. Do not use this client for production code
/// paths — it is for tests only.
pub fn https_test_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        // The one-shot responder closes each connection after a single
        // response, so a pooled idle connection is always dead by the next
        // request. Disabling idle pooling makes the client open a fresh
        // connection per request — matching the responder's
        // one-response-per-connection contract — instead of grabbing a
        // dead pooled socket and depending on reqwest's retry timing, which
        // races under parallel-test CPU load.
        .pool_max_idle_per_host(0)
        .build()
        .expect("build reqwest::Client with invalid-certs override")
}

/// Generate a fresh self-signed cert + key pair valid for `127.0.0.1`
/// and `localhost`, and assemble a `rustls::ServerConfig` around it.
fn build_self_signed_server_config() -> ServerConfig {
    let subject_alt_names = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    let key_pair =
        rcgen::generate_simple_self_signed(subject_alt_names).expect("generate self-signed cert");

    let cert_der: CertificateDer<'static> = key_pair.cert.der().clone();
    let key_der: PrivateKeyDer<'static> = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        key_pair.signing_key.serialize_der(),
    ));

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("build rustls::ServerConfig from self-signed cert")
}

/// Terminate TLS on `stream`, consume one HTTP request, write `resp`,
/// flush, and shut down. Mirrors `responder::serve_one` but layered on
/// `rustls::StreamOwned`.
fn serve_one_tls(stream: TcpStream, config: Arc<ServerConfig>, resp: &str) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let conn = match ServerConnection::new(config) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut tls = StreamOwned::new(conn, stream);
    consume_request(&mut tls);
    let _ = tls.write_all(resp.as_bytes());
    let _ = tls.flush();
    // Send TLS close_notify so the peer's TLS stack (hyper/reqwest)
    // distinguishes a graceful end-of-stream from an aborted
    // connection. Without this, hyper surfaces
    // `UnexpectedEof: peer closed connection without sending TLS
    // close_notify` and the body read fails.
    tls.conn.send_close_notify();
    let _ = tls.conn.write_tls(&mut tls.sock);
    let _ = tls.sock.shutdown(std::net::Shutdown::Both);
}

/// Read the full HTTP request from a generic `Read`er: headers up to
/// the first `\r\n\r\n`, then exactly `Content-Length` bytes of body
/// if that header is present. Same algorithm as
/// [`crate::test_helpers::responder`] — duplicated rather than shared
/// because the plain-HTTP variant works on `&mut TcpStream` directly,
/// while the TLS variant works on `&mut StreamOwned`.
fn consume_request<R: Read>(stream: &mut R) {
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut accum: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    let header_end = loop {
        if Instant::now() >= deadline {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                accum.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_double_crlf(&accum) {
                    break pos + 4;
                }
                if accum.len() > 1 << 20 {
                    return;
                }
            }
            Err(_) => return,
        }
    };

    let content_length = parse_content_length(&accum[..header_end]);
    let already_have = accum.len() - header_end;
    let Some(total_body) = content_length else {
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
            Ok(0) => return,
            Ok(n) => {
                remaining -= n;
            }
            Err(_) => return,
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(header_block: &[u8]) -> Option<usize> {
    let as_str = std::str::from_utf8(header_block).ok()?;
    for line in as_str.split("\r\n") {
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

    /// End-to-end happy path: spin up the HTTPS responder, fetch through
    /// the cert-trusting client, assert body + counter.
    #[tokio::test]
    async fn https_responder_serves_canned_response() {
        let canned = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"remaining\":99}";
        let (addr, calls) = spawn_oneshot_https_responder(vec![canned]);
        let client = https_test_client();
        let url = format!("https://{}/rate_limit", addr);

        let resp = client.get(&url).send().await.expect("send request");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.expect("read body");
        assert_eq!(body, r#"{"remaining":99}"#);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Serve the same response twice — proves the counter increments
    /// per canned response, not per spawn.
    #[tokio::test]
    async fn https_responder_serves_multiple_responses_in_order() {
        let canned_a = "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nA";
        let canned_b = "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nB";
        let (addr, calls) = spawn_oneshot_https_responder(vec![canned_a, canned_b]);
        let client = https_test_client();
        let url = format!("https://{}/x", addr);

        let r1 = client.get(&url).send().await.expect("req 1");
        let b1 = r1.text().await.expect("body 1");
        let r2 = client.get(&url).send().await.expect("req 2");
        let b2 = r2.text().await.expect("body 2");

        assert_eq!(b1, "A");
        assert_eq!(b2, "B");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
