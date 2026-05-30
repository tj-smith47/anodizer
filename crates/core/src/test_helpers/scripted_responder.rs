//! Route-aware multi-response HTTP responder for orchestrator tests.
//!
//! Where [`crate::test_helpers::responder::spawn_oneshot_http_responder`]
//! serves a flat `Vec<&'static str>` in arrival order (regardless of
//! request URL), the scripted variant matches each incoming
//! `(method, path)` pair against a routing table and returns the
//! matching route's canned response. It also records every request
//! (including unmatched ones) so tests can assert on the full
//! interaction afterwards.
//!
//! Designed for testing orchestrators like
//! `crates/stage-release/src/github/mod.rs::run_github_backend`, which
//! performs a sequence of GitHub API calls (find draft, create release,
//! list assets, delete asset, upload asset, ...) in a single end-to-end
//! flow.
//!
//! ## Matching semantics
//!
//! Routes match on **exact** `(method, path)` equality, where `path`
//! includes the query string. A request like
//! `GET /repos/o/r/releases?per_page=100&page=1` matches a route
//! configured with the same string verbatim. If no route matches, the
//! responder writes:
//!
//! ```text
//! HTTP/1.1 404 Not Found
//! Content-Length: 9
//!
//! no route
//! ```
//!
//! and **still records the request in the log** so tests can assert
//! that an unexpected call was made. This is deliberate: silently
//! succeeding on a miss would mask bugs in the system under test.
//!
//! ## Plain HTTP only
//!
//! Production orchestrators reach this responder via the
//! `ANODIZER_GITHUB_API_BASE` (and equivalent) env vars, which the
//! tests redirect to `http://127.0.0.1:<port>`. There is no HTTPS
//! variant of this helper; if a test needs TLS termination, use
//! [`crate::test_helpers::https_responder::spawn_oneshot_https_responder`]
//! instead — but the route-matching surface is intentionally limited to
//! the simpler case.
//!
//! ## Example
//!
//! ```no_run
//! use anodizer_core::test_helpers::scripted_responder::{
//!     spawn_scripted_responder, ScriptedRoute,
//! };
//!
//! let (addr, log) = spawn_scripted_responder(vec![
//!     ScriptedRoute {
//!         method: "GET",
//!         path_pattern: "/repos/o/r/releases?per_page=100&page=1",
//!         response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
//!         times: None,
//!     },
//!     ScriptedRoute {
//!         method: "POST",
//!         path_pattern: "/repos/o/r/releases",
//!         response: "HTTP/1.1 201 Created\r\nContent-Length: 9\r\n\r\n{\"id\":42}",
//!         times: Some(1),
//!     },
//! ]);
//! // ... drive the test against http://{addr}/... then:
//! let entries = log.lock().unwrap();
//! assert_eq!(entries.len(), 2);
//! assert_eq!(entries[0].method, "GET");
//! assert_eq!(entries[1].method, "POST");
//! ```

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Per-connection read timeout. Matches the sibling responders'
/// 5-second budget; see [`crate::test_helpers::responder`] for
/// rationale.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on a single request read.
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);

/// A single route in the scripted routing table.
///
/// `path_pattern` is an exact-match string including the query string.
/// `times` caps how often this route may be served; `None` is unlimited.
/// Once a capped route is exhausted, further requests to that
/// `(method, path)` fall through to the default 404.
#[derive(Debug, Clone)]
pub struct ScriptedRoute {
    /// HTTP method (e.g. `"GET"`, `"POST"`, `"DELETE"`, `"PATCH"`).
    pub method: &'static str,
    /// Request-target (path + optional query string). Exact match.
    pub path_pattern: &'static str,
    /// Full HTTP response (status line + headers + body) to write.
    pub response: &'static str,
    /// Optional cap on how many times this route may be served.
    /// `None` = unlimited; `Some(n)` = serve at most `n` requests
    /// before this route stops matching.
    pub times: Option<usize>,
}

/// One recorded request from the in-process log.
#[derive(Debug, Clone)]
pub struct RequestLog {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// Internal route entry, paired with a hit counter to enforce `times`
/// without mutating the user-facing [`ScriptedRoute`].
struct RouteEntry {
    route: ScriptedRoute,
    hits: AtomicUsize,
}

/// Bind an ephemeral-port TCP listener and serve routed responses
/// indefinitely (until the spawned thread is dropped by accept-error).
///
/// Returns the bound `SocketAddr` and a shared log of every request
/// observed by the responder, including unmatched ones (which receive
/// `404 Not Found`).
///
/// Unlike [`crate::test_helpers::responder::spawn_oneshot_http_responder`],
/// there is no drain phase — orchestrator tests typically make a known
/// number of calls and assert on the log length, so a drain that swallows
/// extra requests would mask bugs rather than tolerate jitter.
pub fn spawn_scripted_responder(
    routes: Vec<ScriptedRoute>,
) -> (SocketAddr, Arc<Mutex<Vec<RequestLog>>>) {
    spawn_scripted_responder_with(|_| routes)
}

/// Variant that lets callers build the route table after the responder
/// has bound its ephemeral port, so response bodies can reference the
/// bound `addr` (e.g., embedding `upload_url: http://<addr>/upload/{id}`
/// in a GitHub release JSON). Avoids the bind-drop-rebind race that
/// would arise if the test pre-bound a port, dropped it, and hoped the
/// responder claimed the same one back.
pub fn spawn_scripted_responder_with<F>(routes_fn: F) -> (SocketAddr, Arc<Mutex<Vec<RequestLog>>>)
where
    F: FnOnce(SocketAddr) -> Vec<ScriptedRoute>,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    spawn_scripted_responder_on(listener, routes_fn)
}

/// Variant that consumes a pre-bound `TcpListener`. Callers that need
/// the bound `addr` *before* constructing routes (e.g., to bake it into
/// response bodies) can bind first, read `local_addr()`, build routes,
/// then hand the listener off — no port-reuse race.
pub fn spawn_scripted_responder_on<F>(
    listener: TcpListener,
    routes_fn: F,
) -> (SocketAddr, Arc<Mutex<Vec<RequestLog>>>)
where
    F: FnOnce(SocketAddr) -> Vec<ScriptedRoute>,
{
    let addr = listener.local_addr().expect("local_addr");
    let log = Arc::new(Mutex::new(Vec::<RequestLog>::new()));
    let log_inner = log.clone();

    let entries: Arc<Vec<RouteEntry>> = Arc::new(
        routes_fn(addr)
            .into_iter()
            .map(|r| RouteEntry {
                route: r,
                hits: AtomicUsize::new(0),
            })
            .collect(),
    );

    std::thread::spawn(move || {
        loop {
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let entries = entries.clone();
            let log = log_inner.clone();
            std::thread::spawn(move || {
                serve_one(stream, &entries, &log);
            });
        }
    });

    (addr, log)
}

/// 404 served on a route miss. Static so the spelling stays stable
/// across the rustdoc + tests.
const NOT_FOUND_RESPONSE: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nno route\n";

fn serve_one(mut stream: TcpStream, entries: &[RouteEntry], log: &Mutex<Vec<RequestLog>>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let (method, path, body) = match consume_request(&mut stream) {
        Some(parsed) => parsed,
        None => return,
    };

    if let Ok(mut g) = log.lock() {
        g.push(RequestLog {
            method: method.clone(),
            path: path.clone(),
            body,
        });
    }

    let response: &str = entries
        .iter()
        .find(|e| {
            e.route.method == method
                && e.route.path_pattern == path
                && match e.route.times {
                    None => true,
                    Some(cap) => e.hits.load(Ordering::SeqCst) < cap,
                }
        })
        .map(|e| {
            e.hits.fetch_add(1, Ordering::SeqCst);
            e.route.response
        })
        .unwrap_or(NOT_FOUND_RESPONSE);

    // Force `Connection: close`: the responder serves exactly one response
    // per connection and then `shutdown`s the socket, so a client (hyper's
    // pool) that keeps the connection alive will reuse an already-closed
    // socket on its next request and surface `connection closed before
    // message completed`. Advertising `close` makes the client open a fresh
    // connection each time, eliminating that reuse race regardless of how
    // many sequential requests a single test drives.
    let _ = write_response_with_connection_close(&mut stream, response);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Write `response` to `stream`, injecting a `Connection: close` header
/// after the status line unless one is already present. Operates on the
/// raw response string so callers keep writing plain HTTP fixtures.
fn write_response_with_connection_close(
    stream: &mut TcpStream,
    response: &str,
) -> std::io::Result<()> {
    if response.to_ascii_lowercase().contains("\r\nconnection:") {
        return stream.write_all(response.as_bytes());
    }
    match response.split_once("\r\n") {
        Some((status_line, rest)) => {
            stream.write_all(status_line.as_bytes())?;
            stream.write_all(b"\r\nConnection: close\r\n")?;
            stream.write_all(rest.as_bytes())
        }
        // No CRLF at all — malformed fixture; write it through unchanged.
        None => stream.write_all(response.as_bytes()),
    }
}

/// Read one HTTP request and return `(method, path, body)`. Returns
/// `None` if the request line never arrived or was malformed —
/// callers treat that as "drop the connection without logging."
fn consume_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut accum: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];

    let header_end = loop {
        if Instant::now() >= deadline {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                accum.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_double_crlf(&accum) {
                    break pos + 4;
                }
                if accum.len() > 1 << 20 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    };

    let header_str = std::str::from_utf8(&accum[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let content_length = parse_content_length(&accum[..header_end]);
    let body_start = header_end;
    let already_have = accum.len() - body_start;

    let body_bytes = match content_length {
        None => Vec::new(),
        Some(total) => {
            if already_have >= total {
                accum[body_start..body_start + total].to_vec()
            } else {
                let mut body = accum[body_start..].to_vec();
                let mut remaining = total - already_have;
                while remaining > 0 {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let want = remaining.min(chunk.len());
                    match stream.read(&mut chunk[..want]) {
                        Ok(0) => break,
                        Ok(n) => {
                            body.extend_from_slice(&chunk[..n]);
                            remaining -= n;
                        }
                        Err(_) => break,
                    }
                }
                body
            }
        }
    };

    let body = String::from_utf8_lossy(&body_bytes).to_string();
    Some((method, path, body))
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
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Helper: open a raw TCP connection to `addr`, send `request`,
    /// read the full response.
    fn send_raw(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("write request");
        stream.flush().expect("flush");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[test]
    fn two_routes_serve_correct_responses_and_log_in_order() {
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/list",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/create",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 9\r\n\r\n{\"id\":42}",
                times: Some(1),
            },
        ]);

        let r1 = send_raw(
            addr,
            "GET /list HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(r1.starts_with("HTTP/1.1 200 OK"), "got: {r1:?}");
        assert!(r1.ends_with("[]"), "got: {r1:?}");

        let r2 = send_raw(
            addr,
            "POST /create HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
        );
        assert!(r2.starts_with("HTTP/1.1 201 Created"), "got: {r2:?}");
        assert!(r2.ends_with("{\"id\":42}"), "got: {r2:?}");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].method, "GET");
        assert_eq!(entries[0].path, "/list");
        assert_eq!(entries[0].body, "");
        assert_eq!(entries[1].method, "POST");
        assert_eq!(entries[1].path, "/create");
        assert_eq!(entries[1].body, "hello");
    }

    #[test]
    fn unknown_route_returns_404_and_still_logs() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/known",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);

        let r = send_raw(
            addr,
            "DELETE /unknown HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(r.starts_with("HTTP/1.1 404 Not Found"), "got: {r:?}");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, "DELETE");
        assert_eq!(entries[0].path, "/unknown");
    }

    #[test]
    fn times_cap_exhausts_route_then_404s() {
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/once",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            times: Some(1),
        }]);

        let r1 = send_raw(
            addr,
            "GET /once HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(
            r1.starts_with("HTTP/1.1 200 OK"),
            "first should hit: {r1:?}"
        );

        let r2 = send_raw(
            addr,
            "GET /once HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(
            r2.starts_with("HTTP/1.1 404 Not Found"),
            "second should 404: {r2:?}"
        );

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
    }
}
