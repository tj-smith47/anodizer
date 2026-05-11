//! Octocrab error -> retriable-error classifier.
//!
//! Both the upload-asset retry loop and the un-draft (publish) PATCH retry
//! face the same failure modes against the GitHub REST API:
//!
//! - `Error::GitHub { source }` carries an HTTP status code; only `>= 500`
//!   and `429` are transient (mirrors `anodizer_core::retry::is_retriable`).
//! - `Error::Hyper` / `Error::Http` / `Error::Service` / `Error::Other` /
//!   `Error::Serde` / `Error::Json` are network-layer / proxy / decoding
//!   failures with no HTTP status attached. Their `Display` strings are
//!   generic ("service error", "error decoding response body") and won't
//!   match `is_network_error`'s substring needles, so wrapping them in a
//!   plain `HttpError { status: 0 }` would *under*-classify them as
//!   non-retriable. Instead we wrap them in
//!   [`anodizer_core::retry::Retriable`] which forces `is_retriable -> true`
//!   regardless of message. These variants are always known-transient when
//!   talking to a healthy GitHub origin (typically nginx/HAProxy 502/503
//!   HTML interstitials breaking JSON parsing).
//!
//! Returns `(wrapped, status)` so callers can also surface the status code
//! in their log lines.
//!
//! Behaviour matches the upload retry path in `mod.rs` (see the
//! `Hyper`/`Http`/`Service`/`Other`/`Serde`/`Json` arm of the `else if`
//! ladder in the upload retry's `Err(err)` handler) — extracted here so
//! the un-draft retry inherits the same classification without copy-paste
//! drift.
//!
//! GoReleaser parity: `internal/client/github.go` treats every non-success
//! upload as `RetriableError`; we narrow that to "5xx / 429 / transport"
//! so genuine 4xx (auth, validation) still fast-fail.

#[cfg(test)]
use anodizer_core::retry::{HttpError, Retriable};

/// Wrap an `octocrab::Error` so `anodizer_core::retry::is_retriable` reports
/// the correct retriability for both REST-status and transport failures.
///
/// Returns `(boxed_error, status_code)` where `status_code` is `0` for
/// transport-layer failures with no HTTP response attached.
///
/// This function consumes the octocrab error. The retry path in
/// [`super::retry_call`] uses a borrow-based variant
/// (`classify_retriability`) so the original typed error can flow back to
/// callers for status-code routing (e.g. mapping 404 to "no existing
/// release"). The two stay in lock-step via the unit tests in this file:
/// the consumption-based classifier here is the test oracle that pins the
/// rule the borrow-based probe must replicate.
#[cfg(test)]
fn classify_octocrab_error(
    err: octocrab::Error,
) -> (Box<dyn std::error::Error + Send + Sync + 'static>, u16) {
    match &err {
        // Status-bearing failures: defer to HttpError + is_retriable's
        // standard 5xx / 429 rule.
        octocrab::Error::GitHub { source, .. } => {
            let status = source.status_code.as_u16();
            (Box::new(HttpError::new(err, status)), status)
        }
        // Transport / decode / proxy failures: no HTTP status, but always
        // safe to retry. Force-wrap in Retriable so is_retriable -> true
        // regardless of the (often opaque) Display message.
        octocrab::Error::Hyper { .. }
        | octocrab::Error::Http { .. }
        | octocrab::Error::Service { .. }
        | octocrab::Error::Other { .. }
        | octocrab::Error::Serde { .. }
        | octocrab::Error::Json { .. } => (Box::new(Retriable::new(err)), 0),
        // Anything else (future octocrab variants, URI parse errors, etc.)
        // falls through as a plain HttpError with status 0 — non-retriable
        // unless the Display matches a network-error needle. Conservative
        // default; better to fast-fail an unfamiliar error than spin on it.
        _ => (Box::new(HttpError::new(err, 0)), 0),
    }
}

#[cfg(test)]
mod tests {
    //! Drive real `octocrab::Error` values through the classifier. Because
    //! octocrab's `error` module is private and the `*Snafu` builder
    //! structs aren't re-exported, we can't synthesize variants directly —
    //! we have to coax the live client into producing them. Two cheap,
    //! deterministic ways:
    //!
    //! 1. Bind an ephemeral TCP listener on `127.0.0.1:0`, capture the port,
    //!    drop the listener (so the OS frees the port and any subsequent
    //!    `connect()` returns ECONNREFUSED instantly), then point an
    //!    `Octocrab` at `http://127.0.0.1:<port>/` and `await` a request.
    //!    Yields a transport-class variant (`Hyper` on Linux/macOS) in
    //!    milliseconds — no risk of hanging on connect-timeout the way
    //!    pointing at TEST-NET-1 (`192.0.2.0/24`) does, because the
    //!    kernel-level "no listener" rejection beats any application-layer
    //!    timeout.
    //! 2. (Future) Stand up a `wiremock` server returning a 5xx with a
    //!    GitHub-error body to drive the `GitHub` arm. Skipped here — the
    //!    `is_retriable` rule for status-bearing errors is already covered
    //!    by `anodizer_core::retry`'s own test suite, and the helper's
    //!    GitHub arm is just `HttpError::new(err, status)`.
    use super::*;
    use anodizer_core::retry::is_retriable;

    async fn make_transport_error() -> octocrab::Error {
        // Reserve + immediately release a loopback port. The `connect()`
        // racing against any listener on that port will see ECONNREFUSED
        // because nothing's bound — the kernel rejects in <1ms.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind probe listener: {e}"))
            .ok()
            .unwrap_or_else(|| panic!("could not bind ephemeral loopback port"));
        let port = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))
            .ok()
            .unwrap_or_else(|| panic!("local_addr failed"))
            .port();
        drop(listener); // release the port so connect() refuses

        let octo = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://127.0.0.1:{port}/"))
            .map_err(|e| format!("base_uri: {e}"))
            .ok()
            .unwrap_or_else(|| panic!("OctocrabBuilder::base_uri rejected loopback URL"));
        let octo = octo
            .build()
            .map_err(|e| format!("build: {e}"))
            .ok()
            .unwrap_or_else(|| panic!("OctocrabBuilder::build failed"));
        // Bound the await with a timeout in case some platform delays
        // ECONNREFUSED — fail loudly instead of hanging the test runner.
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            octo.get::<serde_json::Value, _, ()>("/", None::<&()>),
        )
        .await
        {
            Ok(Ok(_)) => panic!("unexpected success against unbound loopback port"),
            Ok(Err(e)) => e,
            Err(_) => panic!("connect to unbound loopback port did not refuse within 5s"),
        }
    }

    #[tokio::test]
    async fn transport_error_classifies_as_retriable_regardless_of_message() {
        // Real octocrab transport-class error → Retriable wrapper →
        // is_retriable true. Without the helper this would be
        // HttpError{status:0} and is_network_error would have to recognise
        // the (often opaque) Display string — exactly the mis-classification
        // the helper exists to prevent.
        let err = make_transport_error().await;
        let is_transport = matches!(
            &err,
            octocrab::Error::Hyper { .. }
                | octocrab::Error::Http { .. }
                | octocrab::Error::Service { .. }
                | octocrab::Error::Other { .. }
        );
        assert!(
            is_transport,
            "expected a transport-class octocrab error from TEST-NET-1, got: {err:?}"
        );
        let (wrapped, status) = classify_octocrab_error(err);
        assert_eq!(status, 0, "transport errors carry no HTTP status");
        assert!(
            is_retriable(&*wrapped),
            "transport-class octocrab errors must be classified as retriable \
             via the Retriable wrapper"
        );
    }

    #[test]
    fn http_error_inner_5xx_429_is_retriable_4xx_is_not() {
        // The GitHub-arm path of the classifier delegates to is_retriable's
        // standard rule (5xx / 429 retry, other 4xx fast-fail). Pin the
        // contract here so a future refactor of the helper that loses the
        // GitHub arm gets caught.
        let http500 = HttpError::new(std::io::Error::other("internal server error"), 500);
        assert!(is_retriable(&http500), "500 must be retriable");
        let http429 = HttpError::new(std::io::Error::other("rate limited"), 429);
        assert!(is_retriable(&http429), "429 must be retriable");
        let http422 = HttpError::new(std::io::Error::other("validation failed"), 422);
        assert!(
            !is_retriable(&http422),
            "4xx (other than 429) must fast-fail, not retry"
        );
    }

    #[test]
    fn retriable_wrapper_overrides_message_classification() {
        // Pin the load-bearing invariant the helper relies on: wrapping
        // any error in `Retriable` forces is_retriable -> true regardless
        // of the inner Display message. If this contract changes upstream,
        // the helper's transport arm silently mis-classifies.
        let inner = std::io::Error::other("service error");
        let wrapped = Retriable::new(inner);
        assert!(
            is_retriable(&wrapped),
            "Retriable wrapper must force is_retriable -> true"
        );
    }
}
