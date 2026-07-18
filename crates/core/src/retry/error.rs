use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Retriable-error classification
// ---------------------------------------------------------------------------

/// Carries an HTTP status code alongside the original error so
/// [`is_retriable`] can route 5xx / 429 to retry and 4xx to fast-fail.
///
/// HTTP error carrying status + message. Construct via [`HttpError::new`]
/// (status-only) or wrap an existing `reqwest::Response` via
/// [`HttpError::from_response`].
///
/// A `status` of `0` denotes a network-level failure where no response was
/// ever received (the no-response branch). Network-level failures
/// are still classified via the inner error's message, so wrapping them in
/// `HttpError { status: 0, .. }` does not lose retriability information.
#[derive(Debug)]
pub struct HttpError {
    /// The wrapped error (transport, decode, or status-derived message).
    /// Reachable via the [`StdError::source`] trait method (not directly).
    source: Box<dyn StdError + Send + Sync + 'static>,
    /// HTTP status code; `0` for transport-level failures.
    pub status: u16,
}

impl HttpError {
    /// Wrap an error with a status code. `0` denotes a network-level failure
    /// (no response received).
    pub fn new<E>(source: E, status: u16) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
            status,
        }
    }

    /// Wrap a transport-layer error with the status code from the (possibly
    /// missing) response.
    /// `None` resp yields status `0` (network-level failure).
    pub fn from_response<E>(err: E, resp: Option<&reqwest::Response>) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::new(err, resp.map(|r| r.status().as_u16()).unwrap_or(0))
    }
}

/// Extract the upstream HTTP status from an [`anyhow::Error`] chain produced by
/// [`retry_http_blocking`] / [`retry_http_async`].
///
/// Returns `0` when no [`HttpError`] is present in the chain — a transport-level
/// failure that never received a response, or a non-HTTP error.
pub fn http_status(err: &anyhow::Error) -> u16 {
    err.chain()
        .find_map(|e| e.downcast_ref::<HttpError>().map(|h| h.status))
        .unwrap_or(0)
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Defer to the inner error so messages stay focused on the cause.
        // Delegate to the inner error message.
        fmt::Display::fmt(&self.source, f)
    }
}

impl StdError for HttpError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.source)
    }
}

/// Marker error wrapping any inner error so [`is_retriable`] returns `true`
/// regardless of class — useful when a
/// caller knows the failure is transient (e.g. an idempotent registry write
/// returning 422 because of a transient race condition) and wants the retry
/// loop to ignore the usual 4xx fast-fail.
#[derive(Debug)]
pub struct Retriable(Box<dyn StdError + Send + Sync + 'static>);

impl Retriable {
    /// Wrap any error so [`is_retriable`] returns `true` regardless of class.
    /// Use this when a caller knows a 4xx is transient (e.g. a 422 from an
    /// idempotent registry write losing a race) and wants to override the
    /// usual fast-fail. For `Option<E>` inputs, see [`is_retriable_opt`] —
    /// this constructor itself is non-nullable.
    pub fn new<E>(source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self(Box::new(source))
    }
}

impl fmt::Display for Retriable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl StdError for Retriable {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.0)
    }
}

/// Returns `true` if the message looks like a transient network-layer failure.
///
/// Network-error classification, extended for Rust /
/// Windows. Each link in the error chain is checked two ways:
///
/// 1a. **Structural [`io::ErrorKind`] check** via `downcast_ref::<io::Error>()`.
///     Treats `UnexpectedEof`, `TimedOut`, `ConnectionRefused`,
///     `ConnectionReset`, `ConnectionAborted`, and `BrokenPipe` as transient.
///     The OS-classified `ErrorKind` is robust where Display text is not:
///     Linux's connect-refused says `"Connection refused"` but Windows
///     surfaces a transient connect failure as
///     `io::Error { kind: TimedOut, message: "operation timed out" }`, and
///     a Windows-reset reads `"An existing connection was forcibly closed"`.
///     Matching `kind()` catches all of them regardless of phrasing. Also
///     recognises any `io::Error` whose Display form is `"EOF"` /
///     `"unexpected eof"` (rustls / hyper convention; Rust has no
///     equivalent of Go's `io.EOF` sentinel).
///
/// 1b. **Substring match on the lowercased Display form** against
///     [`NETWORK_ERROR_NEEDLES`]. Covers the canonical surface plus the
///     Windows / Rust-stdlib phrasings that bypass the kind check when an
///     error has been wrapped (e.g. reqwest coercing the inner kind to
///     `Other` while preserving the OS message text).
///
/// Walks `.source()` for both branches — Rust's `Display` impls do NOT
/// inherit the wrapped error's text the way Go's `err.Error()` does, so a
/// reqwest "Connection refused" message buried under an anyhow context would
/// otherwise be invisible to the head-only string.
pub fn is_network_error(err: &(dyn StdError + 'static)) -> bool {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        // 1a. Structural ErrorKind check — robust to platform Display drift
        //     (Windows's "operation timed out" vs Linux's "Connection refused").
        if let Some(io_err) = e.downcast_ref::<io::Error>() {
            match io_err.kind() {
                io::ErrorKind::UnexpectedEof
                | io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe => return true,
                _ => {}
            }
            let m = io_err.to_string().to_lowercase();
            if m == "eof" || m == "unexpected eof" {
                return true;
            }
        }

        // 1b. Substring match on each link's own Display (NOT the full
        //     chain "{e:#}" form, which would double-count the same text on
        //     deeper links). Lowercased once per link.
        let s = e.to_string().to_lowercase();
        if NETWORK_ERROR_NEEDLES.iter().any(|n| s.contains(n)) {
            return true;
        }

        cur = e.source();
    }
    false
}

/// The set of substrings classified as transient.
///
/// The first nine entries are the canonical network-error needles
/// (matching is case-insensitive). The remaining entries cover Windows and
/// Rust-stdlib phrasings of transient transport failures that surface when
/// an `io::Error` has been wrapped by a higher layer (reqwest, hyper,
/// anyhow), losing the original `ErrorKind` classification but preserving
/// the OS message text. Without these, every publisher running on Windows
/// fast-failed on the first transient connect blip instead of retrying.
const NETWORK_ERROR_NEEDLES: &[&str] = &[
    "connection reset",
    "network is unreachable",
    "connection closed",
    "connection refused",
    "tls handshake timeout",
    "i/o timeout",
    "broken pipe",
    "timeout awaiting response headers",
    "context deadline exceeded",
    // Windows + macOS phrasing of ErrorKind::TimedOut after wrapping.
    "operation timed out",
    // Windows ErrorKind::ConnectionAborted phrasing.
    "the network connection was aborted",
    // Windows ErrorKind::ConnectionReset phrasing.
    "an existing connection was forcibly closed",
    // hyper-util / reqwest DNS-resolution failures wrapped through the
    // connector. Surfaces as `client error (Connect): dns error: ...` with
    // a platform-specific resolver tail ("Name or service not known" on
    // Linux/glibc, "nodename nor servname provided, or not known" on macOS,
    // "No such host is known" on Windows). The leading "dns error" prefix
    // is the cross-platform constant.
    "dns error",
    // GAI (getaddrinfo) wording across resolvers; covers the Linux
    // resolver tail above and BSD/macOS phrasing.
    "failed to lookup address",
    // Windows resolver tail when DNS-resolution fails.
    "no such host is known",
];

/// Classify an error as retriable.
///
/// Returns `true` for:
/// - any [`is_network_error`] match (substring + EOF / UnexpectedEof in the
///   `source()` chain)
/// - any error whose chain contains a [`Retriable`] wrapper
/// - any error whose chain contains an [`HttpError`] with status `>= 500`
///   or status `429` (Too Many Requests)
///
/// Returns `false` for plain errors and 4xx HTTP errors (other than 429) —
/// those are fast-failed by the retry loop.
pub fn is_retriable(err: &(dyn StdError + 'static)) -> bool {
    // 1. Any link in the chain is an explicit Retriable marker.
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.is::<Retriable>() {
            return true;
        }
        if let Some(http) = e.downcast_ref::<HttpError>()
            && status_is_retriable(http.status)
        {
            return true;
        }
        cur = e.source();
    }

    // 2. Network-error substring / EOF chain match.
    is_network_error(err)
}

/// The canonical retriable-HTTP-status rule: server errors (`>= 500`) and
/// `429 Too Many Requests`. Everything else — notably the remaining 4xx
/// range — is fast-failed.
///
/// [`is_retriable`]'s [`HttpError`] arm delegates here, and raw-status
/// classifiers that cannot route through [`HttpError`] (the gemfury and
/// chocolatey multipart push loops, whose conflict-as-success / hard-fail
/// cases need bespoke `ControlFlow` handling) call it directly, so the
/// fast-fail/retry split for a bare status code has exactly one
/// definition. Extending the rule (408/425, `Retry-After` awareness)
/// updates every consumer at once — including the one-way-door publishers
/// where a mis-fast-failed transient burns an unrecoverable publish
/// attempt.
pub fn status_is_retriable(status: u16) -> bool {
    status >= 500 || status == 429
}

/// Convenience: `None` passes through as `false`. The
/// `IsRetriable(nil) -> false` semantics.
pub fn is_retriable_opt(err: Option<&(dyn StdError + 'static)>) -> bool {
    err.is_some_and(is_retriable)
}

/// Apply ±20 % pseudo-jitter to `base` using a cheap subsecond-nanos modulo.
///
/// Returns a value in `[base * 0.8, base * 1.2)`. No `rand` crate dependency:
/// `SystemTime::now().subsec_nanos()` provides ~nanosecond entropy that is
/// sufficient for retry jitter (the goal is spreading out concurrent retriers,
/// not cryptographic unpredictability).
///
/// The ±20 % window is a widely-adopted convention (AWS SDK, GCP client libs).
/// Jitter only ever widens the sleep by up to 20 %; it never shortens it below
/// 80 % of the nominal delay, so `Retry-After` honoring is conservative.
pub fn jitter_duration(base: Duration) -> Duration {
    let nanos = base.as_nanos() as u64;
    // 20 % of the nominal duration.
    let window = nanos / 5;
    if window == 0 {
        return base;
    }
    // Cheap pseudo-random offset in [0, window * 2) centred on window,
    // giving a net range of [base - window, base + window). The wall-clock
    // seed is XORed with a process-local Weyl sequence (odd-constant atomic
    // counter, so consecutive draws stay well spread) because under
    // SOURCE_DATE_EPOCH a pinned clock would collapse jitter to a constant
    // and re-synchronize concurrent retriers on every round — recreating
    // the exact collision jitter exists to break. SOURCE_DATE_EPOCH pins
    // BUILD OUTPUT bytes; a retry sleep duration never reaches an artifact,
    // so varying it is determinism-safe (which is also why this reads the
    // real clock instead of `sde::resolve_now()`).
    static JITTER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let seq = JITTER_SEQ.fetch_add(0x9E37_79B9_7F4A_7C15, std::sync::atomic::Ordering::Relaxed);
    let seed = clock ^ seq;
    let offset = seed % (window * 2);
    // Saturating arithmetic so we never panic on extreme values.
    let jittered = nanos.saturating_sub(window).saturating_add(offset);
    Duration::from_nanos(jittered)
}
