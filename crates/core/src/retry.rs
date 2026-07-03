//! Uniform retry-with-exponential-backoff primitives.
//!
//! Replaces six open-coded retry loops in `stage-docker` (3×) and
//! `stage-release` (3×) that had diverged on backoff formulas —
//! `2^(n-2)`, `2^(n-1)`, and `500 << (attempt-1)` all coexisted.
//!
//! The canonical policy is exponential backoff with multiplier 2 starting at
//! `base_delay` and capped at `max_delay`:
//!
//! ```text
//! attempt 1:  f() executes immediately
//! attempt 2:  sleep base_delay
//! attempt 3:  sleep base_delay * 2
//! attempt N:  sleep min(base_delay * 2^(N-2), max_delay)
//! ```
//!
//! `ControlFlow<Break, Continue>` lets the operation decide retry policy per
//! failure (e.g. 4xx → Break, 5xx → Continue) without the helper encoding
//! protocol-specific predicates.
//!
//! Both a sync (`retry_sync`) and async (`retry_async`) variant are provided so
//! that sites can adopt without crossing a sync/async boundary.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::time::Duration;

/// Retry policy used by `retry_sync` / `retry_async`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    ///
    /// Invariant: must be `>= 1`. The clamp is enforced at two layers so
    /// every construction path is safe:
    ///
    /// 1. [`crate::config::RetryConfig::to_policy`] clamps user YAML
    ///    (`attempts: 0` -> `1`) at the config-surface boundary.
    /// 2. [`retry_sync`] / [`retry_async`] clamp again at the loop boundary
    ///    to protect direct `RetryPolicy { max_attempts: 0, .. }`
    ///    constructions (e.g. test fixtures).
    ///
    /// Callers therefore do NOT need to clamp `max_attempts` again at the
    /// call site.
    pub max_attempts: u32,
    /// Delay before the second attempt (no wait before the first).
    pub base_delay: Duration,
    /// Upper bound on any individual sleep between attempts.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Canonical upload policy: 10 attempts, 50ms
    /// base, 30s cap.
    pub const UPLOAD: RetryPolicy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(30),
    };

    /// Shallow policy for best-effort pre-publish probes: 3 attempts, 200ms
    /// base, 1s cap.
    ///
    /// Pre-publish probes (token `whoami`, registry index GET, GitHub repo
    /// scope, npm duplicate-version) are an advisory warning gate, not a
    /// write that must land. They run sequentially across every configured
    /// publisher, so the production write-ladder (10 attempts / 10s base /
    /// 5m cap) would let a single wedged endpoint stall the gate for tens of
    /// minutes. A shallow bound keeps the probe responsive while still
    /// absorbing a transient blip; the per-request HTTP timeout still bounds
    /// each individual attempt.
    pub const PREFLIGHT: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
    };

    pub fn delay_for(&self, next_attempt: u32) -> Duration {
        // `next_attempt` is the attempt we're about to run (≥2). The wait
        // before attempt 2 uses base_delay; before attempt 3 uses base_delay*2;
        // i.e. multiplier = 2^(next_attempt - 2).
        let exp = next_attempt.saturating_sub(2);
        let mult = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        let ms = (self.base_delay.as_millis() as u64).saturating_mul(mult);
        std::cmp::min(Duration::from_millis(ms), self.max_delay)
    }

    /// Raise this policy's `max_attempts` to at least [`IDEMPOTENT_PUT_ATTEMPTS`]
    /// without disturbing its backoff shape, returning the adjusted policy.
    ///
    /// An idempotent PUT/POST to a fixed target (an Artifactory/generic upload,
    /// a GemFury push, a Snap Store upload, a bucket blob PUT, a GitHub asset
    /// upload) lands the same bytes at the same path on every re-issue, so a
    /// transient 5xx/429 or dropped connection must retry a bounded number of
    /// times even when a stateful mode (`--publish-only`) resolves the
    /// configured policy down to `attempts: 1`. The floor is a `max()` — it
    /// only widens the bound for the retriable classes and never lowers an
    /// operator-set higher value. 4xx responses still fast-fail inside the
    /// per-attempt classifier regardless of this floor.
    pub fn with_idempotent_floor(self) -> RetryPolicy {
        self.with_floor(IDEMPOTENT_PUT_ATTEMPTS)
    }

    /// Raise this policy's `max_attempts` to at least `min`, leaving the backoff
    /// shape untouched. A `max()` floor, never a clamp that lowers a higher
    /// operator-set value.
    pub fn with_floor(self, min: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.max_attempts.max(min),
            ..self
        }
    }
}

/// Total attempt floor for an idempotent PUT/POST, single-sourcing the
/// "3 total attempts" guarantee shared by every idempotent-upload publisher
/// (HTTP upload, GemFury, Snapcraft, GitHub asset, blob). Applied via
/// [`RetryPolicy::with_idempotent_floor`] as a `max()` so a stateful mode
/// (`--publish-only`) that resolves `attempts: 1` still keeps a bounded
/// transient retry, while an operator-set higher cap is preserved.
pub const IDEMPOTENT_PUT_ATTEMPTS: u32 = 3;

/// Retry a synchronous operation according to `policy`.
///
/// `op` returns:
/// - `Ok(T)` on success (no retry).
/// - `Err(ControlFlow::Continue(e))` to retry if attempts remain.
/// - `Err(ControlFlow::Break(e))` to stop immediately (4xx-style fast-fail).
///
/// Returns the last error if all attempts are exhausted.
pub fn retry_sync<T, E, F>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Result<T, ControlFlow<E, E>>,
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        if attempt > 1 {
            std::thread::sleep(policy.delay_for(attempt));
        }
        match op(attempt) {
            Ok(v) => return Ok(v),
            Err(ControlFlow::Break(e)) => return Err(e),
            Err(ControlFlow::Continue(e)) => {
                if attempt >= max {
                    return Err(e);
                }
            }
        }
        attempt += 1;
    }
}

/// Retry an asynchronous operation according to `policy`.
///
/// Same semantics as `retry_sync` but awaits `op` and uses `tokio::time::sleep`.
pub async fn retry_async<T, E, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControlFlow<E, E>>>,
{
    let max = policy.max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        if attempt > 1 {
            tokio::time::sleep(policy.delay_for(attempt)).await;
        }
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(ControlFlow::Break(e)) => return Err(e),
            Err(ControlFlow::Continue(e)) => {
                if attempt >= max {
                    return Err(e);
                }
            }
        }
        attempt += 1;
    }
}

/// Whether to consider 3xx redirects a success outcome (most upload-style
/// publishers do, since the underlying client follows redirects under the
/// hood; some callers explicitly want only 2xx).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccessClass {
    /// 2xx only. Any 3xx is treated as a non-success status (eligible for
    /// retry / fast-fail per `is_retriable`).
    Strict,
    /// 2xx OR 3xx. Used by upload publishers whose servers may emit a
    /// 301/302/307 in the success path (artifactory does this for some
    /// virtual repo configurations).
    AllowRedirects,
}

/// Drive a single HTTP call to completion, retrying transient failures via
/// the shared [`retry_sync`] machinery.
///
/// On every attempt, `send` is invoked to construct + dispatch a fresh
/// request. The closure must rebuild the request from scratch (multipart
/// `Form`, streamed body, etc. are move-only). The helper:
///
/// 1. On `Err` (transport-level): wrap in [`HttpError::from_response`] +
///    a `<label>: <stage> transport error` context, classify with
///    [`is_retriable`] (so EOF / connection-reset retry, plain "dial
///    failed" fast-fails), and dispatch `Continue`/`Break`.
/// 2. On non-success status: drain the body, format the outer message via
///    `error_msg`, wrap in [`HttpError::new`] with the upstream status, and
///    classify (5xx/429 → `Continue`, 4xx → `Break`).
/// 3. On success status: return `(status, body)`.
///
/// The `error_msg` closure receives the response status and body so callers
/// can format publisher-specific envelopes (e.g. artifactory's
/// `{"errors":[...]}` JSON).
///
/// Replaces three nearly-identical retry loops:
/// - `stage-publish/cloudsmith.rs::retry_request`
/// - `stage-publish/artifactory.rs::upload_single_artifact` (inline)
/// - `stage-announce/helpers.rs::retry_http` (now wraps this helper; see
///   announce/helpers.rs for the thin adapter that returns the body string
///   instead of `(StatusCode, String)`).
pub fn retry_http_blocking<F, M>(
    label: &str,
    policy: &RetryPolicy,
    success_class: SuccessClass,
    mut send: F,
    error_msg: M,
) -> anyhow::Result<(reqwest::StatusCode, String)>
where
    F: FnMut(u32) -> Result<reqwest::blocking::Response, reqwest::Error>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    use anyhow::Context as _;
    retry_sync(policy, |attempt| {
        match send(attempt) {
            Ok(resp) => {
                let status = resp.status();
                let succeeded = match success_class {
                    SuccessClass::Strict => status.is_success(),
                    SuccessClass::AllowRedirects => status.is_success() || status.is_redirection(),
                };
                let body = resp
                    .text()
                    .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
                if succeeded {
                    Ok((status, body))
                } else {
                    let msg = error_msg(status, &body);
                    let inner = anyhow::anyhow!("{msg}");
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(inner.to_string()),
                        status.as_u16(),
                    ))
                    .context(inner);
                    // `as_ref()` is the head of the chain; `is_retriable` walks
                    // `.source()` to reach `HttpError`. `root_cause()` would
                    // unwrap past `HttpError` to the io::Error leaf and miss
                    // the status. Pinned by
                    // `classifier_5xx_via_anyhow_chain_uses_as_ref`.
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
            Err(e) => {
                // Transport-layer failure: always wrap in HttpError(status=0)
                // so the chain-walking classifier can see network-error
                // substrings via the inner io::Error message.
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context(format!("{label}: HTTP transport error"));
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
        }
    })
    .with_context(|| format!("{label}: exhausted retry attempts"))
}

/// Async sibling of [`retry_http_blocking`] for `reqwest::Client` (non-blocking)
/// call sites such as the GitLab and Gitea release publishers.
///
/// Each attempt invokes `send` (a fresh future) and:
///
/// 1. On `Err` (transport-level): wraps in [`HttpError::from_response`] +
///    a `<label>: HTTP transport error` context, classifies via
///    [`is_retriable`] (network-substring + EOF chain match), and dispatches
///    `Continue`/`Break`.
/// 2. On non-success status: drains the body via `Response::text().await`,
///    formats the outer message via `error_msg`, wraps in [`HttpError::new`]
///    with the upstream status, and classifies (5xx/429 → `Continue`, 4xx →
///    `Break`).
/// 3. On success status: returns the raw [`reqwest::Response`] for the
///    caller to consume (e.g. `.json()`, `.text()`, header inspection).
///
/// `success_class` mirrors the blocking variant: `Strict` rejects 3xx,
/// `AllowRedirects` accepts them. Most async API clients want `Strict`
/// (their reqwest::Client follows redirects by default, so a surfaced 3xx
/// is itself an error).
pub async fn retry_http_async<F, Fut, M>(
    label: &str,
    policy: &RetryPolicy,
    success_class: SuccessClass,
    mut send: F,
    error_msg: M,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    use anyhow::Context as _;
    retry_async(policy, |attempt| {
        let fut = send(attempt);
        let error_msg = &error_msg;
        async move {
            match fut.await {
                Ok(resp) => {
                    let status = resp.status();
                    let succeeded = match success_class {
                        SuccessClass::Strict => status.is_success(),
                        SuccessClass::AllowRedirects => {
                            status.is_success() || status.is_redirection()
                        }
                    };
                    if succeeded {
                        Ok(resp)
                    } else {
                        let body = resp
                            .text()
                            .await
                            .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
                        let msg = error_msg(status, &body);
                        let inner = anyhow::anyhow!("{msg}");
                        let wrapped = anyhow::Error::new(HttpError::new(
                            std::io::Error::other(inner.to_string()),
                            status.as_u16(),
                        ))
                        .context(inner);
                        // `as_ref()` is the head of the chain; `is_retriable`
                        // walks `.source()` to reach `HttpError`. `root_cause()`
                        // would unwrap past `HttpError` to the io::Error leaf
                        // and miss the status. Pinned by
                        // `classifier_5xx_via_anyhow_chain_uses_as_ref`.
                        if is_retriable(wrapped.as_ref()) {
                            Err(ControlFlow::Continue(wrapped))
                        } else {
                            Err(ControlFlow::Break(wrapped))
                        }
                    }
                }
                Err(e) => {
                    // Transport-layer failure: wrap in HttpError(status=0) so
                    // the chain-walking classifier can see network-error
                    // substrings via the inner io::Error message.
                    let err = anyhow::Error::new(HttpError::from_response(e, None))
                        .context(format!("{label}: HTTP transport error"));
                    if is_retriable(err.as_ref()) {
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err))
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!("{label}: exhausted retry attempts"))
}

/// Classify a `reqwest::Result<reqwest::blocking::Response>` into the
/// `ControlFlow` shape expected by `retry_sync` for a typical HTTP call:
/// 5xx + transport errors retry, 4xx fast-fails, 2xx/3xx returns Ok. The
/// returned response (Ok branch) is the caller's to consume.
///
/// This is the convention shared by every HTTP-uploading publisher; see audit
/// A7 dedup S5.
pub fn classify_http_sync(
    result: reqwest::Result<reqwest::blocking::Response>,
) -> Result<reqwest::blocking::Response, ControlFlow<anyhow::Error, anyhow::Error>> {
    use anyhow::anyhow;
    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.is_redirection() {
                Ok(resp)
            } else if status.is_server_error() {
                Err(ControlFlow::Continue(anyhow!(
                    "HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("server error")
                )))
            } else {
                // 4xx (and any other non-success/redirect/5xx): fast-fail
                Err(ControlFlow::Break(anyhow!(
                    "HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("client error")
                )))
            }
        }
        // Transport-layer failure (DNS, connect, TLS, timeout): retry.
        Err(e) => Err(ControlFlow::Continue(anyhow!(e))),
    }
}

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
    // giving a net range of [base - window, base + window). Routed
    // through `sde::resolve_now()` so jitter collapses to a constant
    // under `SOURCE_DATE_EPOCH` (no subsec precision) — required for
    // determinism-harness byte-equality; real jitter is preserved in
    // prod where the helper falls back to `Utc::now()`.
    let seed = crate::sde::resolve_now().timestamp_subsec_nanos() as u64;
    let offset = seed % (window * 2);
    // Saturating arithmetic so we never panic on extreme values.
    let jittered = nanos.saturating_sub(window).saturating_add(offset);
    Duration::from_nanos(jittered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    /// Locks the shallow shape of the best-effort pre-publish probe policy so a
    /// future edit cannot silently re-point preflight probes at the production
    /// write-ladder (10 attempts / 10s base / 5m cap), which would let one
    /// wedged endpoint stall the gate for tens of minutes.
    #[test]
    fn preflight_policy_is_shallow() {
        let p = RetryPolicy::PREFLIGHT;
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.base_delay, Duration::from_millis(200));
        assert_eq!(p.max_delay, Duration::from_secs(1));
        // Sub-second base + low cap: the whole probe ladder must stay well
        // under a second of sleeps even when every attempt is exhausted.
        let total_sleep: Duration = (2..=p.max_attempts).map(|n| p.delay_for(n)).sum();
        assert!(
            total_sleep < Duration::from_secs(1),
            "preflight backoff sleeps must stay sub-second, got {total_sleep:?}"
        );
    }

    #[test]
    fn http_status_extracts_status_from_chain() {
        let wrapped = anyhow::Error::new(HttpError::new(std::io::Error::other("boom"), 429))
            .context("outer context");
        assert_eq!(http_status(&wrapped), 429);
    }

    #[test]
    fn http_status_is_zero_without_http_error() {
        let plain = anyhow::anyhow!("not an http error");
        assert_eq!(http_status(&plain), 0);
    }

    /// The idempotent floor raises a sub-floor cap to [`IDEMPOTENT_PUT_ATTEMPTS`]
    /// but never lowers an operator-set higher cap. Fails if the floor constant
    /// is reverted to 1 (or the `max()` semantics flip to a clamp).
    #[test]
    fn idempotent_floor_raises_low_cap_and_preserves_high_cap() {
        let raised = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
        .with_idempotent_floor();
        assert_eq!(
            raised.max_attempts, IDEMPOTENT_PUT_ATTEMPTS,
            "a single-attempt cap must be raised to the idempotent floor"
        );

        let preserved = RetryPolicy {
            max_attempts: 7,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
        .with_idempotent_floor();
        assert_eq!(
            preserved.max_attempts, 7,
            "an operator-set cap above the floor must be preserved, not lowered"
        );
    }

    #[test]
    fn jitter_returns_base_when_window_rounds_to_zero() {
        // For any duration under 5ns the ±20 % window (`nanos / 5`) floors to
        // 0, so jitter is a no-op and the base is returned unchanged — the
        // early-return guard that avoids a `% 0` panic on tiny delays.
        for n in 0..5u64 {
            let base = Duration::from_nanos(n);
            assert_eq!(
                jitter_duration(base),
                base,
                "sub-5ns base {n} must pass through unjittered"
            );
        }
    }

    #[test]
    fn jitter_stays_within_plus_minus_twenty_percent() {
        // The jittered value never leaves [base*0.8, base*1.2) — the documented
        // window. Uses a duration large enough that `nanos / 5 > 0`.
        let base = Duration::from_millis(100);
        let jittered = jitter_duration(base);
        let lo = base.mul_f64(0.8);
        let hi = base.mul_f64(1.2);
        assert!(
            jittered >= lo && jittered < hi,
            "jittered {jittered:?} outside [{lo:?}, {hi:?})"
        );
    }

    #[test]
    fn delay_progression_caps_at_max() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        };
        assert_eq!(p.delay_for(2), Duration::from_millis(100));
        assert_eq!(p.delay_for(3), Duration::from_millis(200));
        assert_eq!(p.delay_for(4), Duration::from_millis(400));
        assert_eq!(p.delay_for(5), Duration::from_millis(500)); // capped
        assert_eq!(p.delay_for(8), Duration::from_millis(500)); // capped
    }

    #[test]
    fn sync_succeeds_on_first_attempt() {
        let calls = AtomicU32::new(0);
        let result: Result<&str, ()> = retry_sync(&fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("ok")
        });
        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sync_retries_until_success() {
        let calls = AtomicU32::new(0);
        let result: Result<u32, &str> = retry_sync(&fast_policy(), |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            if attempt < 3 {
                Err(ControlFlow::Continue("transient"))
            } else {
                Ok(attempt)
            }
        });
        assert_eq!(result, Ok(3));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn sync_break_stops_immediately() {
        let calls = AtomicU32::new(0);
        let result: Result<(), &str> = retry_sync(&fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ControlFlow::Break("fatal"))
        });
        assert_eq!(result, Err("fatal"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sync_returns_last_error_after_exhaustion() {
        let calls = AtomicU32::new(0);
        let result: Result<(), String> = retry_sync(&fast_policy(), |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ControlFlow::Continue(format!("fail {attempt}")))
        });
        assert_eq!(result, Err("fail 4".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn async_retries_until_success() {
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let calls_inner = calls.clone();
        let result: Result<u32, &str> = retry_async(&fast_policy(), move |attempt| {
            let c = calls_inner.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(ControlFlow::Continue("transient"))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result, Ok(2));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // -----------------------------------------------------------------------
    // is_network_error / is_retriable / HttpError / Retriable
    //
    // Network-error classification test cases.
    // -----------------------------------------------------------------------

    /// Plain string error wrapper used in classification tests.
    #[derive(Debug)]
    struct StrErr(&'static str);
    impl fmt::Display for StrErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl StdError for StrErr {}

    #[derive(Debug)]
    struct OwnedErr(String);
    impl fmt::Display for OwnedErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl StdError for OwnedErr {}

    #[test]
    fn network_error_substrings_match() {
        for s in [
            "connection reset by peer",
            "network is unreachable",
            "connection closed unexpectedly",
            "connection refused",
            "tls handshake timeout",
            "i/o timeout",
            "CONNECTION RESET",
            "TLS Handshake Timeout",
            "write: broken pipe",
            "net/http: timeout awaiting response headers",
            "context deadline exceeded",
            // DNS-resolution failures across platforms (hyper-util connector
            // surfaces these via reqwest as `client error (Connect): dns
            // error: <platform tail>`). Pin every tail we know about so a
            // cross-platform CI failure cannot reintroduce the gap.
            "client error (Connect): dns error: failed to lookup address information: Name or service not known",
            "dns error: nodename nor servname provided, or not known",
            "dns error: No such host is known. (os error 11001)",
        ] {
            let e = OwnedErr(s.to_string());
            assert!(is_network_error(&e), "expected network error: {s:?}");
        }
    }

    #[test]
    fn network_error_io_eof_kinds() {
        let e = io::Error::from(io::ErrorKind::UnexpectedEof);
        assert!(is_network_error(&e));

        // A custom-kind io::Error whose Display is "EOF" (rustls / hyper convention).
        let e2 = io::Error::other("EOF");
        assert!(is_network_error(&e2));
    }

    // Windows-CI regression: connect() on Windows surfaces transient failures
    // as io::Error { kind: TimedOut, message: "operation timed out" }, neither
    // of which matched the original EOF-only kind check or the
    // needle list. Same shape for the connection-* kinds across platforms —
    // pin each branch.

    #[test]
    fn is_network_error_classifies_io_timedout() {
        let e = io::Error::from(io::ErrorKind::TimedOut);
        assert!(is_network_error(&e));
        assert!(is_retriable(&e));
    }

    #[test]
    fn is_network_error_classifies_io_connection_refused() {
        let e = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(is_network_error(&e));
        assert!(is_retriable(&e));
    }

    #[test]
    fn is_network_error_classifies_io_connection_reset() {
        let e = io::Error::from(io::ErrorKind::ConnectionReset);
        assert!(is_network_error(&e));
        assert!(is_retriable(&e));
    }

    #[test]
    fn is_network_error_classifies_io_connection_aborted() {
        let e = io::Error::from(io::ErrorKind::ConnectionAborted);
        assert!(is_network_error(&e));
        assert!(is_retriable(&e));
    }

    #[test]
    fn is_network_error_classifies_io_broken_pipe() {
        let e = io::Error::from(io::ErrorKind::BrokenPipe);
        assert!(is_network_error(&e));
        assert!(is_retriable(&e));
    }

    #[test]
    fn is_network_error_classifies_operation_timed_out_substring() {
        // Simulate a reqwest- or hyper-wrapped error whose io::ErrorKind has
        // been coerced to Other but whose Display still carries the Windows /
        // macOS TimedOut phrasing. Both the substring path and the
        // ErrorKind path must classify this independently.
        let other_kind = io::Error::other("operation timed out");
        assert!(is_network_error(&other_kind));
        assert!(is_retriable(&other_kind));

        let kind_only = io::Error::from(io::ErrorKind::TimedOut);
        assert!(is_network_error(&kind_only));
        assert!(is_retriable(&kind_only));
    }

    #[test]
    fn network_error_wrapped_unexpected_eof() {
        // Wrap an UnexpectedEof in an outer error so chain-walking is exercised.
        #[derive(Debug)]
        struct Wrap(io::Error);
        impl fmt::Display for Wrap {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "read failed")
            }
        }
        impl StdError for Wrap {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(&self.0)
            }
        }
        let inner = io::Error::from(io::ErrorKind::UnexpectedEof);
        let outer = Wrap(inner);
        assert!(is_network_error(&outer));
    }

    #[test]
    fn network_error_non_network_strings_reject() {
        for s in [
            "file not found",
            "permission denied",
            "dial tcp: lookup example.com: no such host",
            "",
        ] {
            let e = OwnedErr(s.to_string());
            assert!(!is_network_error(&e), "expected NOT network error: {s:?}");
        }
    }

    #[test]
    fn retriable_opt_nil_passthrough() {
        assert!(!is_retriable_opt(None));
    }

    #[test]
    fn http_error_500_retriable() {
        let e = HttpError::new(StrErr("internal server error"), 500);
        assert!(is_retriable(&e));
    }

    #[test]
    fn http_error_502_503_retriable() {
        for s in [502u16, 503] {
            let e = HttpError::new(StrErr("bad gateway"), s);
            assert!(is_retriable(&e), "status {s} should be retriable");
        }
    }

    #[test]
    fn http_error_429_retriable() {
        let e = HttpError::new(StrErr("rate limited"), 429);
        assert!(is_retriable(&e));
    }

    #[test]
    fn http_error_4xx_not_retriable() {
        for s in [400u16, 401, 403, 404, 422] {
            let e = HttpError::new(StrErr("client err"), s);
            assert!(!is_retriable(&e), "status {s} should NOT be retriable");
        }
    }

    #[test]
    fn http_error_zero_status_routes_via_message() {
        // Status 0 == network-level failure with no response. Retriability
        // falls back to the network-error substring matcher on the inner.
        let net = HttpError::new(StrErr("connection reset"), 0);
        assert!(is_retriable(&net));

        let non_net = HttpError::new(StrErr("dial failed"), 0);
        assert!(!is_retriable(&non_net));
    }

    #[test]
    fn http_error_unwrap_chain_visible() {
        let inner = StrErr("inner");
        let e = HttpError::new(inner, 503);
        assert!(e.source().is_some());
    }

    #[test]
    fn from_response_nil_resp_yields_status_zero() {
        // No response means status 0.
        // Use a concrete `io::Error` since `reqwest::Error` cannot be
        // synthesised in tests; the API accepts any `E: StdError + Send + Sync`.
        let inner = io::Error::other("connect: dial tcp");
        let e = HttpError::from_response(inner, None);
        assert_eq!(e.status, 0);
    }

    #[test]
    fn from_response_unwrap_chain_visible() {
        // The inner error must remain reachable via the StdError chain so
        // is_retriable's network-error matcher can still see the cause.
        let inner = io::Error::other("connection reset by peer");
        let e = HttpError::from_response(inner, None);
        assert!(
            e.source().is_some(),
            "inner error must be reachable via source()"
        );
        // And classification must walk through to the network-error matcher.
        assert!(is_retriable(&e));
    }

    #[test]
    fn retriable_wrapper_is_retriable() {
        let e = Retriable::new(StrErr("retry me"));
        assert!(is_retriable(&e));
    }

    #[test]
    fn retriable_wrapper_overrides_4xx() {
        // A 422 wrapped in Retriable is still retriable.
        let inner = HttpError::new(StrErr("exists"), 422);
        let outer = Retriable::new(inner);
        assert!(is_retriable(&outer));
    }

    #[test]
    fn retriable_wrapper_unwrap_chain_visible() {
        let inner = StrErr("inner");
        let e = Retriable::new(inner);
        assert!(e.source().is_some());
    }

    #[test]
    fn plain_error_not_retriable() {
        let e = StrErr("something");
        assert!(!is_retriable(&e));
    }

    #[test]
    fn anyhow_error_threadable() {
        // Ensure is_retriable works through anyhow::Error's deref-to-dyn path
        // (which is the canonical caller form across the codebase).
        let e: anyhow::Error = anyhow::anyhow!("connection refused");
        assert!(is_retriable(e.as_ref()));

        let e2: anyhow::Error = anyhow::anyhow!("permission denied");
        assert!(!is_retriable(e2.as_ref()));
    }

    #[test]
    fn is_retriable_chain_walks_to_http_error() {
        // An anyhow::Error wrapping a concrete HttpError must be classified
        // by walking source(), not by Display alone — the message "outer"
        // gives no hint, the 503 status does.
        let inner = HttpError::new(StrErr("bad gateway"), 503);
        let wrapped: anyhow::Error = anyhow::Error::new(inner).context("publish failed");
        assert!(is_retriable(wrapped.as_ref()));
    }

    // ----- as_ref vs root_cause drift guard ---------------------------------
    //
    // Every consumer of `retry_http_blocking` (artifactory, cloudsmith, the
    // future stage-blob upload paths) classifies via `is_retriable(err.as_ref())`.
    // A subtle but catastrophic regression is to "simplify" that to
    // `is_retriable(err.root_cause())`, which walks past the HttpError wrapper
    // to the leaf io::Error — at which point 5xx misclassifies as fast-fail
    // (the leaf has no status code), and the entire retry policy becomes a
    // no-op. These tests pin the distinction once at the helper's home.

    #[test]
    fn classifier_5xx_via_anyhow_chain_uses_as_ref() {
        let wrapped: anyhow::Error =
            anyhow::Error::new(HttpError::new(std::io::Error::other("503"), 503))
                .context("publish");
        assert!(
            is_retriable(wrapped.as_ref()),
            "5xx HttpError reached via as_ref() must classify retriable"
        );
    }

    #[test]
    fn classifier_root_cause_walks_past_http_error_drift_guard() {
        // Drift guard: root_cause() unwraps to the leaf io::Error, which
        // has no status. If a future caller ever swaps as_ref → root_cause
        // they'll regress 5xx retry handling. This assertion locks the
        // distinction.
        let wrapped: anyhow::Error =
            anyhow::Error::new(HttpError::new(std::io::Error::other("503"), 503))
                .context("publish");
        assert!(
            !is_retriable(wrapped.root_cause()),
            "root_cause() walks past HttpError; 5xx must NOT be detected via the leaf"
        );
    }

    #[test]
    fn classifier_429_via_anyhow_chain_uses_as_ref() {
        // Symmetry with the 5xx case: 429 is the other retriable status
        // class and must also stay reachable via as_ref().
        let wrapped: anyhow::Error =
            anyhow::Error::new(HttpError::new(std::io::Error::other("429"), 429))
                .context("publish");
        assert!(is_retriable(wrapped.as_ref()));
        assert!(!is_retriable(wrapped.root_cause()));
    }

    // ----- retry_http_blocking behavioural tests ---------------------------
    //
    // `reqwest::Error` has no public constructor, so the transport-error
    // branch is exercised indirectly via per-publisher integration tests
    // (which mock at the network layer). The unit tests here drive a tiny
    // hand-rolled TCP server so we can exercise the success / non-success
    // status branches with a real reqwest::blocking::Client end-to-end.

    use crate::test_helpers::responder::spawn_oneshot_http_responder;

    #[test]
    fn retry_http_blocking_success_returns_first_attempt() {
        let (addr, calls) =
            spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_blocking(
            "test",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |_, _| String::from("should not be called on success"),
        );
        let (status, body) = result.expect("success");
        assert_eq!(status.as_u16(), 200);
        assert_eq!(body, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "single attempt");
    }

    #[test]
    fn retry_http_blocking_retries_5xx_then_succeeds() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_blocking(
            "test",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |status, body| format!("{status}: {body}"),
        );
        let (status, _) = result.expect("eventually succeeds");
        assert_eq!(status.as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry then success");
    }

    #[test]
    fn retry_http_blocking_4xx_fast_fails_no_retry() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
        ]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_blocking(
            "myscope",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |status, body| format!("custom error: {status} body={body}"),
        );
        let err = result.expect_err("4xx must fast-fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("custom error"),
            "error formatter must be invoked on non-success; got: {chain}"
        );
        assert!(chain.contains("404"), "status must be in chain: {chain}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "4xx must NOT retry (only one connection accepted)"
        );
    }

    #[test]
    fn retry_http_blocking_redirect_class_alters_success_predicate() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
        ]);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            // Disable redirect-following so the 307 surfaces to our helper.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_blocking(
            "test",
            &policy,
            SuccessClass::AllowRedirects,
            |_| client.get(format!("http://{addr}/")).send(),
            |_, _| String::from("should not be called on 3xx with AllowRedirects"),
        );
        let (status, _) = result.expect("3xx is success under AllowRedirects");
        assert_eq!(status.as_u16(), 307);
    }

    // ----- retry_http_async behavioural tests ------------------------------
    //
    // Mirrors the blocking suite but drives an async reqwest::Client against
    // the same hand-rolled TCP responder (running on a worker thread, so the
    // tokio reactor is free to drive the client futures). The transport-error
    // arm (Err(reqwest::Error)) is exercised by
    // `retry_http_{async,blocking}_transport_error_retries_then_fails` below,
    // which bind an ephemeral port, drop the listener, then point the client
    // at the now-defunct address.

    #[tokio::test]
    async fn retry_http_async_success_returns_first_attempt() {
        let (addr, calls) =
            spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_async(
            "test",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |_, _| String::from("should not be called on success"),
        )
        .await;
        let resp = result.expect("success");
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.text().await.expect("body");
        assert_eq!(body, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "single attempt");
    }

    #[tokio::test]
    async fn retry_http_async_retries_5xx_then_succeeds() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_async(
            "test",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |status, body| format!("{status}: {body}"),
        )
        .await;
        let resp = result.expect("eventually succeeds");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry then success");
    }

    #[tokio::test]
    async fn retry_http_async_4xx_fast_fails_no_retry() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
        ]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_async(
            "myscope",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |status, body| format!("custom error: {status} body={body}"),
        )
        .await;
        let err = result.expect_err("4xx must fast-fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("custom error"),
            "error formatter must be invoked on non-success; got: {chain}"
        );
        assert!(chain.contains("404"), "status must be in chain: {chain}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "4xx must NOT retry (only one connection accepted)"
        );
    }

    #[tokio::test]
    async fn retry_http_async_429_retries_then_succeeds() {
        // 429 (Too Many Requests) is the second retriable class alongside
        // 5xx. Ensures the helper doesn't accidentally fast-fail on rate
        // limits — a regression here would defeat the whole point of
        // wiring retry into release publishers.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_async(
            "test",
            &policy,
            SuccessClass::Strict,
            |_| client.get(format!("http://{addr}/")).send(),
            |status, body| format!("{status}: {body}"),
        )
        .await;
        let resp = result.expect("429 retried then success");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // ----- transport-error behavioural tests -------------------------------
    //
    // The transport-error arm (Err(reqwest::Error): DNS failure, connection
    // refused, EOF, TLS handshake failure, etc.) is the single most
    // reviewer-load-bearing path: it is the one the helper claims to retry
    // and that publishers rely on for resilience against transient network
    // blips. The pattern below dials the RFC 2606-reserved `.invalid` TLD,
    // which is guaranteed never to resolve, so every attempt fails at the
    // DNS-resolution stage in a few milliseconds on Linux, macOS, and
    // Windows alike.
    //
    // We verify:
    //   1. the helper retries (attempt counter > 1)
    //   2. eventually surfaces an Err with the configured label in the chain
    // The outer attempt counter is incremented inside the closure, so it
    // sees one bump per attempt regardless of the underlying transport
    // outcome.
    //
    // RFC 2606 (https://datatracker.ietf.org/doc/html/rfc2606) reserves the
    // `.invalid` TLD precisely for this purpose; using it removes any
    // dependence on OS-level TCP semantics (Windows' kernel can retransmit
    // SYN against an unbound loopback port until the connect timeout fires
    // rather than refusing synchronously like Linux + macOS do).
    const TRANSPORT_FAIL_URL: &str = "http://nonexistent.invalid/";

    #[test]
    fn retry_http_blocking_transport_error_retries_then_fails() {
        let attempts = std::sync::Arc::new(AtomicU32::new(0));
        let attempts_inner = attempts.clone();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_blocking(
            "test-transport",
            &policy,
            SuccessClass::Strict,
            |_| {
                attempts_inner.fetch_add(1, Ordering::SeqCst);
                client.get(TRANSPORT_FAIL_URL).send()
            },
            |_, _| String::from("non-success branch should not be reached"),
        );
        let err = result.expect_err("transport error must surface as Err");
        let chain = format!("{err:#}");
        assert!(
            attempts.load(Ordering::SeqCst) > 1,
            "transport error must be retried; got {} attempts; chain={chain}",
            attempts.load(Ordering::SeqCst)
        );
        assert!(
            chain.contains("test-transport"),
            "label must surface in error chain; got: {chain}"
        );
    }

    #[tokio::test]
    async fn retry_http_async_transport_error_retries_then_fails() {
        let attempts = std::sync::Arc::new(AtomicU32::new(0));
        let attempts_inner = attempts.clone();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("client");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result = retry_http_async(
            "test-transport-async",
            &policy,
            SuccessClass::Strict,
            |_| {
                attempts_inner.fetch_add(1, Ordering::SeqCst);
                client.get(TRANSPORT_FAIL_URL).send()
            },
            |_, _| String::from("non-success branch should not be reached"),
        )
        .await;
        let err = result.expect_err("transport error must surface as Err");
        assert!(
            attempts.load(Ordering::SeqCst) > 1,
            "transport error must be retried; got {} attempts",
            attempts.load(Ordering::SeqCst)
        );
        let chain = format!("{err:#}");
        assert!(
            chain.contains("test-transport-async"),
            "label must surface in error chain; got: {chain}"
        );
    }
}
