//! Per-attempt classifier for the GitHub release-asset upload loop.
//!
//! The per-asset upload loop in `upload.rs` retries a single asset upload
//! through several distinct failure modes (server error, GitHub
//! `already_exists` 422, secondary rate-limit, primary rate-limit, plain
//! transport flake, ...). Each mode demands a different action (delete &
//! retry, sleep the secondary-RL backoff, probe `/rate_limit`, sleep an
//! exponential-backoff slot, surface the error). The decision itself —
//! "given this `octocrab::Error`, what kind of attempt outcome is it?" —
//! is a pure function of the error shape, independent of the surrounding
//! stateful machinery (`last_err`, `overwrite_attempted`, `sleep`s,
//! `delete_release_asset_by_name`).
//!
//! [`classify_upload_attempt`] is that pure function. The loop body in
//! `upload.rs` matches on the returned [`UploadAttemptOutcome`] and runs the
//! appropriate stateful arm. Splitting the predicate out keeps the action
//! arms readable (no nested `matches!` chains) and makes the classification
//! rules unit-testable against synthesized `octocrab::Error` values.

use super::secondary_rate_limit::is_secondary_rate_limit;

/// Coarse classification of a single upload attempt's result.
///
/// Variants correspond to the distinct action arms in the upload retry
/// loop. The loop body owns all stateful behaviour (sleeps, deletes,
/// error propagation); this enum only names *which* arm to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadAttemptOutcome {
    /// Upload succeeded — leave the retry loop.
    Success,
    /// GitHub returned `422 Unprocessable Entity` with a body whose
    /// `errors[].code` includes `"already_exists"`. The loop probes the
    /// remote asset's size and either skips (byte-identical), bails
    /// (different bytes + `replace_existing_artifacts: false`), or
    /// deletes and retries.
    AlreadyExists,
    /// GitHub returned a 403/429 whose body matches the secondary
    /// rate-limit signature (see
    /// [`is_secondary_rate_limit`](super::secondary_rate_limit::is_secondary_rate_limit)).
    /// The loop honours `Retry-After` (clamped) and sleeps before
    /// retrying, NOT the normal exponential backoff.
    SecondaryRateLimited,
    /// GitHub returned a 403/429 that does NOT match the secondary
    /// rate-limit signature. Treated as primary quota exhaustion: the
    /// loop probes `/rate_limit` and sleeps until the quota resets.
    PrimaryRateLimited,
    /// GitHub returned `404 Not Found`. Immediately after a release is
    /// created this is the post-create read-after-write replication lag:
    /// octocrab's `upload_asset(...).send()` first does a
    /// `GET /releases/{id}` to read `upload_url`, and that read can hit a
    /// replica that has not yet observed the create. The asset was
    /// definitively NOT created, so re-issuing the upload is
    /// idempotent-safe. The loop sleeps an exponential-backoff slot and
    /// retries — bounded by [`MIN_UPLOAD_TRANSIENT_ATTEMPTS`] even in
    /// stateful modes whose policy caps attempts at 1, so a genuinely
    /// missing release still fails once the floor is exhausted.
    ///
    /// [`MIN_UPLOAD_TRANSIENT_ATTEMPTS`]: super::upload::MIN_UPLOAD_TRANSIENT_ATTEMPTS
    NotFound,
    /// A 5xx response, a `401` from the uploads endpoint, or a
    /// transport-layer failure (`Hyper`, `Http`, `Service`, `Other`,
    /// `Serde`, `Json`). The loop sleeps an exponential-backoff slot
    /// and retries.
    ///
    /// `401` is transient HERE (upload path only): uploads.github.com
    /// intermittently rejects a valid token with `401 Bad credentials`
    /// — observed mid-release after the same token had just uploaded
    /// dozens of assets. A genuinely bad token still fails after the
    /// bounded attempts; it just costs a few backoff slots instead of
    /// aborting a half-uploaded release on a flake.
    ///
    /// `Serde`/`Json` are classified as transient because GitHub will
    /// occasionally return an HTML 502/503 page from an upstream proxy
    /// while octocrab's error mapping expects a JSON body; treating
    /// those as fatal would surface bogus parse errors on otherwise
    /// transient failures.
    TransientRetry,
    /// Any other error — validation 4xx other than 422
    /// already-exists, or an unrecognised variant. The loop surfaces
    /// the error immediately.
    Fatal,
}

/// Classify the result of a single `upload_asset(...).send().await` call.
///
/// Pure function: no I/O, no state mutation. The upload loop owns the
/// "what to do next" (sleep, delete-and-retry, return-err); this just
/// names the bucket the error belongs to.
///
/// Precedence — when multiple predicates could match, the first match
/// in this order wins (matches the historical inline behaviour):
///   1. `Ok(_)` → [`UploadAttemptOutcome::Success`]
///   2. GitHub 422 with `errors[].code == "already_exists"` →
///      [`UploadAttemptOutcome::AlreadyExists`]
///   3. GitHub 404 → [`UploadAttemptOutcome::NotFound`]
///   4. Secondary rate-limit signature on a 403/429 →
///      [`UploadAttemptOutcome::SecondaryRateLimited`]
///   5. Plain 403 or 429 →
///      [`UploadAttemptOutcome::PrimaryRateLimited`]
///   6. 5xx, 401, or `Hyper` / `Http` / `Service` / `Other` / `Serde` /
///      `Json` variants → [`UploadAttemptOutcome::TransientRetry`]
///   7. Anything else → [`UploadAttemptOutcome::Fatal`]
///
/// The success arm carries no payload because the loop only needs to know
/// "did this attempt succeed?" — the uploaded asset metadata is not
/// inspected after a successful upload.
pub(crate) fn classify_upload_attempt<T>(
    result: &Result<T, octocrab::Error>,
) -> UploadAttemptOutcome {
    let err = match result {
        Ok(_) => return UploadAttemptOutcome::Success,
        Err(e) => e,
    };

    // 422 + already_exists: most specific 4xx the loop handles, must be
    // checked before the generic primary-rate-limit fall-through.
    let is_already_exists = matches!(
        err,
        octocrab::Error::GitHub { source, .. }
            if source.status_code.as_u16() == 422
                && source.errors.as_ref().is_some_and(|errs| {
                    errs.iter().any(|e| {
                        e.get("code").and_then(|v| v.as_str()) == Some("already_exists")
                    })
                })
    );
    if is_already_exists {
        return UploadAttemptOutcome::AlreadyExists;
    }

    // 404 right after create is read-after-write replication lag, not a
    // missing release. Checked before the rate-limit / server-error
    // buckets (a 404 matches none of them) so it gets the dedicated
    // bounded-retry arm rather than falling through to Fatal.
    let is_not_found = matches!(
        err,
        octocrab::Error::GitHub { source, .. }
            if source.status_code.as_u16() == 404
    );
    if is_not_found {
        return UploadAttemptOutcome::NotFound;
    }

    if is_secondary_rate_limit(err) {
        return UploadAttemptOutcome::SecondaryRateLimited;
    }

    let is_primary_rate_limited = matches!(
        err,
        octocrab::Error::GitHub { source, .. }
            if source.status_code.as_u16() == 403
                || source.status_code.as_u16() == 429
    );
    if is_primary_rate_limited {
        return UploadAttemptOutcome::PrimaryRateLimited;
    }

    // 401 from the uploads endpoint is a known transient flake (see the
    // TransientRetry variant doc); bounded retry, not fast-fail.
    let is_transient_status = matches!(
        err,
        octocrab::Error::GitHub { source, .. }
            if source.status_code.is_server_error()
                || source.status_code.as_u16() == 401
    );
    if is_transient_status
        || matches!(err, octocrab::Error::Hyper { .. })
        || matches!(err, octocrab::Error::Http { .. })
        || matches!(err, octocrab::Error::Service { .. })
        || matches!(err, octocrab::Error::Other { .. })
        || matches!(err, octocrab::Error::Serde { .. })
        || matches!(err, octocrab::Error::Json { .. })
    {
        return UploadAttemptOutcome::TransientRetry;
    }

    UploadAttemptOutcome::Fatal
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    /// Synthesize an `octocrab::Error::GitHub` for a chosen status and body.
    ///
    /// octocrab's `*Snafu` builders are private, so the canonical path to
    /// a typed `Error::GitHub { source, .. }` is to drive a real HTTP
    /// response through the client and capture the `Err`. For 429 / 5xx
    /// responses octocrab's tower retry middleware makes up to 3
    /// additional attempts, so the responder must serve the response
    /// 4 times for those statuses; for other 4xx statuses 1 is enough.
    async fn synth_github_error(status: u16, body: &str) -> octocrab::Error {
        let body_len = body.len();
        let raw = format!(
            "HTTP/1.1 {status} STATUS\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {body_len}\r\n\
             \r\n\
             {body}"
        );
        let static_resp: &'static str = Box::leak(raw.into_boxed_str());
        let serve_count: usize = if status == 429 || status >= 500 { 4 } else { 1 };
        let (addr, _calls) = spawn_oneshot_http_responder(vec![static_resp; serve_count]);
        // Pin rustls to `ring` before octocrab builds its reqwest client; the
        // graph links two providers and nextest isolates each test in its own
        // process. See `crate::test_support::build_test_octocrab`.
        anodizer_core::tls::install_default_crypto_provider();
        let octo = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://{addr}/"))
            .expect("base_uri")
            .build()
            .expect("build");
        octo.get::<serde_json::Value, _, _>("/test", None::<&()>)
            .await
            .expect_err("synth_github_error: octocrab must surface Err for non-2xx status")
    }

    fn ok_result() -> Result<serde_json::Value, octocrab::Error> {
        Ok(serde_json::json!({}))
    }

    #[tokio::test]
    async fn ok_classifies_as_success() {
        assert_eq!(
            classify_upload_attempt(&ok_result()),
            UploadAttemptOutcome::Success,
        );
    }

    #[tokio::test]
    async fn github_422_already_exists_classifies_as_already_exists() {
        let body = r#"{"message":"Validation Failed","errors":[{"resource":"ReleaseAsset","code":"already_exists","field":"name"}]}"#;
        let err = synth_github_error(422, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::AlreadyExists,
        );
    }

    #[tokio::test]
    async fn github_422_other_code_is_fatal() {
        // A 422 whose errors[].code is something other than "already_exists"
        // (e.g. a validation error on a different field) must NOT be
        // classified as AlreadyExists — there's no delete-and-retry recovery
        // path for it. It also doesn't match any of the transient buckets,
        // so it falls through to Fatal.
        let body = r#"{"message":"Validation Failed","errors":[{"resource":"ReleaseAsset","code":"invalid","field":"name"}]}"#;
        let err = synth_github_error(422, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::Fatal,
        );
    }

    #[tokio::test]
    async fn github_404_classifies_as_not_found() {
        // A 404 immediately after release create is GitHub's post-create
        // read-after-write replication lag (the GET inside
        // upload_asset(...).send() hit a replica that hasn't observed the
        // create yet). It must NOT fall through to Fatal: the upload loop
        // retries it, bounded, so the release is not killed by a single
        // transient miss.
        let body = r#"{"message":"Not Found"}"#;
        let err = synth_github_error(404, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::NotFound,
        );
    }

    #[tokio::test]
    async fn github_500_classifies_as_transient_retry() {
        let body = r#"{"message":"Server Error"}"#;
        let err = synth_github_error(500, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::TransientRetry,
        );
    }

    #[tokio::test]
    async fn github_503_classifies_as_transient_retry() {
        let body = r#"{"message":"Service Unavailable"}"#;
        let err = synth_github_error(503, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::TransientRetry,
        );
    }

    #[tokio::test]
    async fn github_403_with_secondary_rl_body_classifies_as_secondary() {
        let body = r#"{"message":"You have exceeded a secondary rate limit and have been temporarily blocked from content creation. Please retry your request again later.","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let err = synth_github_error(403, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::SecondaryRateLimited,
        );
    }

    #[tokio::test]
    async fn github_403_without_secondary_rl_body_classifies_as_primary() {
        let body =
            r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#;
        let err = synth_github_error(403, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::PrimaryRateLimited,
        );
    }

    #[tokio::test]
    async fn github_401_classifies_as_transient_retry() {
        // The v0.9.0 incident: uploads.github.com returned a one-off
        // `401 Bad credentials` after the same token had uploaded 84
        // assets, and the fast-fail killed the whole release. 401 on
        // the upload path is bounded-transient, never Fatal.
        let body =
            r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#;
        let err = synth_github_error(401, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::TransientRetry,
        );
    }

    #[tokio::test]
    async fn github_400_classifies_as_fatal() {
        // Non-transient 4xx (not 401/403/404/422/429) must still
        // fast-fail: there is no recovery path for a malformed request.
        let body = r#"{"message":"Bad Request"}"#;
        let err = synth_github_error(400, body).await;
        let result: Result<serde_json::Value, octocrab::Error> = Err(err);
        assert_eq!(
            classify_upload_attempt(&result),
            UploadAttemptOutcome::Fatal,
        );
    }
}
