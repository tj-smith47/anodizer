use super::*;
use std::ops::ControlFlow;

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
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    success_class: SuccessClass,
    send: F,
    error_msg: M,
) -> anyhow::Result<(reqwest::StatusCode, String)>
where
    F: FnMut(u32) -> Result<reqwest::blocking::Response, reqwest::Error>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    retry_http_blocking_deadline(rlog, policy, None, success_class, send, error_msg)
}

/// Like [`retry_http_blocking`], but stops once the next backoff would push
/// total wall-time past `deadline` (from [`crate::Context::retry_deadline`]), so
/// a long upload storm exits resumable before the outer job timeout instead of
/// running the full attempt ladder. `deadline: None` is the unbounded form.
pub fn retry_http_blocking_deadline<F, M>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    success_class: SuccessClass,
    mut send: F,
    error_msg: M,
) -> anyhow::Result<(reqwest::StatusCode, String)>
where
    F: FnMut(u32) -> Result<reqwest::blocking::Response, reqwest::Error>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    use anyhow::Context as _;
    retry_sync_deadline(rlog, policy, deadline, |attempt| {
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
                    .context(format!("{}: HTTP transport error", rlog.desc()));
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
        }
    })
    .with_context(|| format!("{}: exhausted retry attempts", rlog.desc()))
}

/// Binary-body sibling of [`retry_http_blocking`] for endpoints whose success
/// payload is not valid UTF-8 (e.g. a gzip-compressed `.crate` tarball).
///
/// `resp.text()` runs a lossy UTF-8 conversion that silently rewrites
/// non-UTF-8 byte sequences to U+FFFD, corrupting a binary payload with no
/// error raised — a caller hashing the "recovered" bytes would never match
/// the original digest. This variant reads the success body via
/// `resp.bytes()` instead, keeping every other behavior (retry classification,
/// `HttpError` wrapping, `success_class`) identical to the text variant. The
/// error-path body is still decoded lossily into text purely for the
/// `error_msg` formatter — error responses are conventionally textual/JSON,
/// and a few replacement characters in an already-failing message are
/// harmless.
pub fn retry_http_blocking_bytes<F, M>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    success_class: SuccessClass,
    send: F,
    error_msg: M,
) -> anyhow::Result<(reqwest::StatusCode, Vec<u8>)>
where
    F: FnMut(u32) -> Result<reqwest::blocking::Response, reqwest::Error>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    retry_http_blocking_bytes_deadline(rlog, policy, None, success_class, send, error_msg)
}

/// Deadline-bounded sibling of [`retry_http_blocking_bytes`], mirroring
/// [`retry_http_blocking_deadline`] for binary success bodies. `deadline: None`
/// is the unbounded form.
pub fn retry_http_blocking_bytes_deadline<F, M>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    success_class: SuccessClass,
    mut send: F,
    error_msg: M,
) -> anyhow::Result<(reqwest::StatusCode, Vec<u8>)>
where
    F: FnMut(u32) -> Result<reqwest::blocking::Response, reqwest::Error>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    use anyhow::Context as _;
    retry_sync_deadline(rlog, policy, deadline, |attempt| match send(attempt) {
        Ok(resp) => {
            let status = resp.status();
            let succeeded = match success_class {
                SuccessClass::Strict => status.is_success(),
                SuccessClass::AllowRedirects => status.is_success() || status.is_redirection(),
            };
            let bytes = resp
                .bytes()
                .map(|b| b.to_vec())
                .unwrap_or_else(|e| format!("<failed to read body: {e}>").into_bytes());
            if succeeded {
                Ok((status, bytes))
            } else {
                let body_text = String::from_utf8_lossy(&bytes).into_owned();
                let msg = error_msg(status, &body_text);
                let inner = anyhow::anyhow!("{msg}");
                let wrapped = anyhow::Error::new(HttpError::new(
                    std::io::Error::other(inner.to_string()),
                    status.as_u16(),
                ))
                .context(inner);
                if is_retriable(wrapped.as_ref()) {
                    Err(ControlFlow::Continue(wrapped))
                } else {
                    Err(ControlFlow::Break(wrapped))
                }
            }
        }
        Err(e) => {
            let err = anyhow::Error::new(HttpError::from_response(e, None))
                .context(format!("{}: HTTP transport error", rlog.desc()));
            if is_retriable(err.as_ref()) {
                Err(ControlFlow::Continue(err))
            } else {
                Err(ControlFlow::Break(err))
            }
        }
    })
    .with_context(|| format!("{}: exhausted retry attempts", rlog.desc()))
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
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    success_class: SuccessClass,
    send: F,
    error_msg: M,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    M: Fn(reqwest::StatusCode, &str) -> String,
{
    retry_http_async_deadline(rlog, policy, None, success_class, send, error_msg).await
}

/// Deadline-bounded sibling of [`retry_http_async`], the async counterpart of
/// [`retry_http_blocking_deadline`], so async upload publishers (GitLab/Gitea
/// release-asset uploads) honor the [`crate::Context::retry_deadline`] budget.
/// `deadline: None` is the unbounded form.
pub async fn retry_http_async_deadline<F, Fut, M>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
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
    retry_async_deadline(rlog, policy, deadline, |attempt| {
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
                        .context(format!("{}: HTTP transport error", rlog.desc()));
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
    .with_context(|| format!("{}: exhausted retry attempts", rlog.desc()))
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
