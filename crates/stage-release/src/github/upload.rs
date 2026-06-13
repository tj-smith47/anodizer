//! Single-asset upload with bounded transient retry.
//!
//! [`upload_release_asset`] is the body of the per-asset task spawned by
//! `run_github_backend`'s parallel upload loop, lifted out of `backend.rs`
//! so the retry/recovery state machine is unit-testable against a scripted
//! HTTP responder (octocrab's `upload_asset(...).send()` issues a real
//! `GET /releases/{id}` + upload POST per attempt, both of which a loopback
//! responder can serve).
//!
//! The retry classes (see [`classify_upload_attempt`]):
//!
//! - `TransientRetry` (5xx / 401 / transport / decode) and `NotFound`
//!   (post-create read-after-write 404) sleep an exponential-backoff slot
//!   and retry, bounded by `max(policy.max_attempts,`
//!   [`MIN_UPLOAD_TRANSIENT_ATTEMPTS`]`)`.
//! - `AlreadyExists` (422) probes the remote asset's size + state and
//!   dispatches skip / bail / delete-and-retry via
//!   [`classify_already_exists`]. A PARTIAL asset (GitHub state not
//!   `"uploaded"` — debris from an interrupted prior attempt, e.g. one of
//!   the transient failures above) is always deleted and re-uploaded,
//!   regardless of `replace_existing_artifacts`.
//! - `SecondaryRateLimited` sleeps the dedicated secondary-RL delay;
//!   `PrimaryRateLimited` probes `/rate_limit` and waits for quota.
//! - `Fatal` (remaining 4xx, unknown variants) surfaces immediately.

use std::sync::Arc;

use anodizer_core::retry::{RetryPolicy, jitter_duration};
use anyhow::{Context as _, Result};

use super::secondary_rate_limit::{RetryAfterCapture, secondary_rl_delay_with_env};
use super::spec::{AlreadyExistsAction, classify_already_exists, upload_retry_locals};
use super::upload_outcome::{UploadAttemptOutcome, classify_upload_attempt};
use super::{
    check_github_rate_limit_with_env, delete_release_asset_by_name, find_release_asset_probe,
    format_retry_warn,
};
use crate::release_log;

/// Guaranteed minimum number of upload attempts for the transient /
/// read-after-write-404 classes, even when the resolved [`RetryPolicy`]
/// caps `max_attempts` at 1 (as stateful modes like `--publish-only` do).
///
/// A 404 from `upload_asset` immediately after the release was created is
/// GitHub's post-create read-after-write replication lag, not a missing
/// release — the asset definitively was not created, so re-issuing the
/// upload is idempotent-safe. Genuinely-missing releases still fail once
/// this floor is exhausted.
///
/// The floor is applied as `max(configured_attempts, this)` on the SHARED
/// upload retry loop's iteration cap, so it raises the bound for the whole
/// loop, not just the transient/404 classes. It is the per-class fast-fail
/// arms inside the loop (Fatal / 422-bail) that still terminate on
/// the first attempt — those outcomes never consume the extra iterations
/// this floor makes available.
pub(crate) const MIN_UPLOAD_TRANSIENT_ATTEMPTS: u32 = 3;

/// Everything one asset upload needs, borrowed from the backend's
/// per-task captures. Grouped so the call site stays readable and the
/// function signature doesn't trip `too_many_arguments`.
pub(crate) struct UploadAssetRequest<'a> {
    pub octo: &'a Arc<octocrab::Octocrab>,
    pub owner: &'a str,
    pub repo: &'a str,
    pub release_id: u64,
    pub tag: &'a str,
    pub path: &'a std::path::Path,
    pub file_name: &'a str,
    pub replace_existing_artifacts: bool,
    pub policy: &'a RetryPolicy,
    pub retry_after: Option<&'a RetryAfterCapture>,
    /// Token forwarded to the primary-rate-limit `/rate_limit` probe.
    pub token: &'a str,
    pub env_source: &'a dyn anodizer_core::EnvSource,
}

/// Upload one artifact to the release, retrying transient failures and
/// recovering 422 `already_exists` conflicts (idempotent skip, partial-
/// asset delete-and-retry, or opted-in overwrite).
///
/// Immutable-releases policy: never pre-emptively delete a published
/// asset. The 422 `already_exists` arm probes the asset's size + state
/// and dispatches Skip / Bail / DeleteAndRetry via
/// [`classify_already_exists`] — that is the only delete site for an
/// already-published asset.
pub(crate) async fn upload_release_asset(req: UploadAssetRequest<'_>) -> Result<()> {
    let UploadAssetRequest {
        octo,
        owner,
        repo,
        release_id,
        tag,
        path,
        file_name,
        replace_existing_artifacts,
        policy,
        retry_after,
        token,
        env_source,
    } = req;

    // Retry parameters come from the resolved policy: `attempts` caps the
    // loop, `delay`/`max_delay` shape the exponential backoff. The loop
    // body is bespoke (resume-stream + 422 already-exists handling); only
    // the knobs are user-configurable. The `>= 1` clamp lives at
    // `RetryConfig::to_policy` (see `RetryPolicy::max_attempts` rustdoc);
    // no additional clamp is needed here.
    let (configured_attempts, initial_retry_delay, max_retry_delay) = upload_retry_locals(policy);
    // The transient / read-after-write-404 classes get a guaranteed floor
    // of attempts even when stateful modes (e.g. `--publish-only`) resolve
    // `max_attempts` to 1: a single post-create 404 or upload flake is
    // recoverable and must not kill the whole release. The Fatal /
    // 422-bail arms still fast-fail regardless of this floor.
    let max_upload_attempts = std::cmp::max(configured_attempts, MIN_UPLOAD_TRANSIENT_ATTEMPTS);

    let mut last_err: Option<anyhow::Error> = None;
    // One-shot overwrite guard: once a stale PUBLISHED asset has been
    // successfully deleted and the upload *still* hits `already_exists`,
    // give up gracefully instead of looping. This happens when GitHub's
    // release-asset delete is eventually consistent: the delete returns Ok
    // immediately but the subsequent upload still sees the stale asset for
    // a short window. Rather than burn the remaining retries (and
    // ultimately fail the whole release), accept the stale bytes and move
    // on. PARTIAL assets are exempt from this guard — accepting one would
    // leave a corrupt, non-downloadable asset on the release — so they
    // keep delete-retrying until the attempt budget is exhausted.
    let mut overwrite_attempted = false;
    for attempt in 1..=max_upload_attempts {
        let data = std::fs::read(path)
            .with_context(|| format!("release: read artifact {}", path.display()))?;
        let local_size = data.len() as u64;

        let result = octo
            .repos(owner, repo)
            .releases()
            .upload_asset(release_id, file_name, data.into())
            .send()
            .await;
        let outcome = classify_upload_attempt(&result);
        match outcome {
            UploadAttemptOutcome::Success => {
                last_err = None;
                break;
            }
            UploadAttemptOutcome::AlreadyExists => {
                let err = result.expect_err("AlreadyExists outcome guarantees Err variant");

                // Probe the remote asset's size + state to distinguish
                // "same bytes uploaded earlier" (idempotent no-op),
                // "partial from an interrupted attempt" (always delete +
                // retry), and "different bytes, user opted out of
                // overwrites" (unrecoverable). The classifier
                // [`classify_already_exists`] encodes the 422 decision
                // rule.
                let probe = find_release_asset_probe(
                    octo,
                    owner,
                    repo,
                    release_id,
                    file_name,
                    policy,
                    retry_after,
                )
                .await
                .with_context(|| {
                    format!(
                        "release: look up existing asset '{}' on release '{}'",
                        file_name, tag
                    )
                })?;
                let partial = probe.is_some_and(|p| !p.uploaded);

                // Eventual-consistency tolerance for PUBLISHED assets only:
                // if a prior iteration already deleted the stale asset and
                // GitHub still reports `already_exists`, keep the stale
                // bytes rather than fail the release. A reappearing
                // PARTIAL must keep retrying instead — skipping would
                // leave a corrupt asset behind.
                if overwrite_attempted && !partial {
                    release_log().warn(&format!(
                        "existing asset '{file_name}' on release '{tag}' \
                         reappeared after delete+retry; \
                         skipping, stale asset kept"
                    ));
                    last_err = None;
                    break;
                }

                match classify_already_exists(replace_existing_artifacts, probe, local_size) {
                    AlreadyExistsAction::SkipIdempotent => {
                        // A prior attempt in this same release already
                        // uploaded byte-identical content. Pure no-op,
                        // regardless of `replace_existing_artifacts`.
                        last_err = None;
                        break;
                    }
                    AlreadyExistsAction::BailReplaceForbidden => {
                        // User explicitly set
                        // `replace_existing_artifacts: false`
                        // and the bytes differ: surface the
                        // conflict rather than overwriting.
                        // Treated as an unrecoverable error.
                        return Err(anyhow::anyhow!(err)).with_context(|| {
                            format!(
                                "release: artifact '{}' already exists on release '{}' \
                                 with different bytes and `replace_existing_artifacts: false` \
                                 forbids overwriting (set \
                                 `release.replace_existing_artifacts: true` \
                                 to permit overwrites)",
                                file_name, tag
                            )
                        });
                    }
                    AlreadyExistsAction::DeleteAndRetry => {
                        // Fall through to the delete-retry arm below:
                        // either a partial from an interrupted attempt
                        // (always recoverable) or a size mismatch the
                        // user opted into overwriting via
                        // `replace_existing_artifacts: true`.
                    }
                }

                // Delete the conflicting asset and retry. If the delete
                // itself fails (perms, asset disappeared mid-flight,
                // etc.), warn and treat the upload as skipped for a
                // published asset (a stale asset is better than aborting
                // the release); a partial that cannot be deleted is a
                // hard error — it is not downloadable, so "keep it" is
                // not an option.
                match delete_release_asset_by_name(
                    octo,
                    owner,
                    repo,
                    release_id,
                    file_name,
                    policy,
                    retry_after,
                )
                .await
                {
                    Ok(_) => {
                        if !partial {
                            overwrite_attempted = true;
                        }
                        last_err = Some(anyhow::anyhow!(err));
                        if attempt < max_upload_attempts {
                            let base = std::cmp::min(
                                initial_retry_delay * 2u32.pow(attempt - 1),
                                max_retry_delay,
                            );
                            tokio::time::sleep(jitter_duration(base)).await;
                        }
                        continue;
                    }
                    Err(del_err) if partial => {
                        return Err(del_err).with_context(|| {
                            format!(
                                "release: delete partial asset '{}' on release '{}' \
                                 left by an interrupted upload",
                                file_name, tag
                            )
                        });
                    }
                    Err(del_err) => {
                        release_log().warn(&format!(
                            "could not overwrite existing asset '{file_name}' on release '{tag}' \
                             (size mismatch and delete failed: {del_err}); skipping, stale asset kept"
                        ));
                        last_err = None;
                        break;
                    }
                }
            }
            UploadAttemptOutcome::SecondaryRateLimited => {
                // Secondary rate-limit (403/429 with GitHub's
                // secondary-RL body): sleep the dedicated RL
                // delay (with ±20 % jitter) before retrying. Do
                // NOT fall through to the primary
                // `check_github_rate_limit` path — secondary
                // limits are transient burst guards, not quota
                // exhaustion.
                let err = result.expect_err("SecondaryRateLimited outcome guarantees Err variant");
                // Read the secondary-RL delay through the request-scoped
                // `env_source` (not the global process env) so the
                // `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` override is honored
                // without a global-env race between parallel callers/tests.
                let delay = jitter_duration(secondary_rl_delay_with_env(retry_after, env_source));
                release_log().warn(&format!(
                    "upload of '{file_name}' hit GitHub secondary \
                     rate limit; sleeping {:.1}s before retry \
                     (attempt {attempt}/{})",
                    delay.as_secs_f64(),
                    max_upload_attempts,
                ));
                if attempt < max_upload_attempts {
                    tokio::time::sleep(delay).await;
                }
                last_err = Some(anyhow::anyhow!(err));
                continue;
            }
            UploadAttemptOutcome::PrimaryRateLimited => {
                // Primary rate-limit (403/429 without the
                // secondary-RL body): probe `/rate_limit` and
                // sleep until quota resets.
                let err = result.expect_err("PrimaryRateLimited outcome guarantees Err variant");
                release_log().status(&format!(
                    "rate limited on upload of '{file_name}', checking rate limits..."
                ));
                check_github_rate_limit_with_env(&reqwest::Client::new(), token, 100, env_source)
                    .await;
                last_err = Some(anyhow::anyhow!(err));
                continue;
            }
            UploadAttemptOutcome::NotFound => {
                // octocrab's `upload_asset(...).send()` does a
                // `GET /releases/{id}` (to read `upload_url`)
                // before the POST; right after the create that
                // read can hit a GitHub replica lagging the
                // create, yielding a transient 404. The asset
                // was definitively not created, so retrying is
                // idempotent-safe. Bounded by
                // `max_upload_attempts` (floored at
                // MIN_UPLOAD_TRANSIENT_ATTEMPTS) so a genuinely
                // missing release still fails once exhausted.
                let err = result.expect_err("NotFound outcome guarantees Err variant");
                let label = format!("upload of '{file_name}'");
                // NotFound is by construction a 404, so the
                // status is a literal here rather than extracted
                // from the error as the TransientRetry arm does.
                release_log().warn(&format_retry_warn(
                    &label,
                    attempt,
                    max_upload_attempts,
                    404,
                ));
                last_err = Some(anyhow::anyhow!(err));
                if attempt < max_upload_attempts {
                    let base =
                        std::cmp::min(initial_retry_delay * 2u32.pow(attempt - 1), max_retry_delay);
                    tokio::time::sleep(jitter_duration(base)).await;
                }
                continue;
            }
            UploadAttemptOutcome::TransientRetry => {
                // Transient transport / proxy / auth-flake issues during
                // upload. Serde / Json here means GitHub returned a
                // non-JSON body (typically an nginx/HAProxy 502/503 HTML
                // page) while the error-mapping expected JSON; 401 is the
                // uploads-endpoint "Bad credentials" flake: always
                // transient, safe to retry bounded. Route the per-attempt
                // warn through the shared `format_retry_warn` helper so
                // this bespoke loop cannot drift from the
                // `retry_octocrab_call` helper's format.
                let err = result.expect_err("TransientRetry outcome guarantees Err variant");
                let status = match &err {
                    octocrab::Error::GitHub { source, .. } => source.status_code.as_u16(),
                    _ => 0,
                };
                let label = format!("upload of '{file_name}'");
                release_log().warn(&format_retry_warn(
                    &label,
                    attempt,
                    max_upload_attempts,
                    status,
                ));
                last_err = Some(anyhow::anyhow!(err));
                if attempt < max_upload_attempts {
                    let base =
                        std::cmp::min(initial_retry_delay * 2u32.pow(attempt - 1), max_retry_delay);
                    tokio::time::sleep(jitter_duration(base)).await;
                }
                continue;
            }
            UploadAttemptOutcome::Fatal => {
                // Non-retryable error: fail immediately.
                let err = result.expect_err("Fatal outcome guarantees Err variant");
                return Err(anyhow::anyhow!(err)).with_context(|| {
                    format!(
                        "release: upload artifact '{}' to release '{}'",
                        file_name, tag
                    )
                });
            }
        }
    }
    if let Some(err) = last_err {
        return Err(err).with_context(|| {
            format!(
                "release: upload artifact '{}' to release '{}' failed after {} attempts",
                file_name, tag, max_upload_attempts
            )
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Drive the full per-asset retry loop end-to-end against a scripted
    //! loopback responder. octocrab's `upload_asset(...).send()` performs a
    //! real `GET /releases/{id}` (to read `upload_url`) followed by the
    //! upload POST, so each "attempt" consumes two scripted responses; the
    //! GET's release fixture points `upload_url` back at the responder.
    //!
    //! Scripted statuses deliberately avoid 429/5xx so octocrab's internal
    //! tower retry middleware (which transparently retries those) cannot
    //! consume responses out from under the loop-level assertions.
    use super::*;
    use crate::test_support::{build_test_octocrab, test_retry_policy};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;

    /// Minimal env source for the request struct; the rate-limit arms are
    /// never reached by these scripts.
    struct EmptyEnv;
    impl anodizer_core::EnvSource for EmptyEnv {
        fn var(&self, _key: &str) -> Option<String> {
            None
        }
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// `GET /releases/{id}` fixture whose `upload_url` targets the
    /// responder itself, so the follow-up POST lands on the next scripted
    /// response.
    fn release_json(addr: SocketAddr) -> String {
        let body = format!(
            r#"{{
                "url": "http://{addr}/repos/o/r/releases/7",
                "html_url": "http://{addr}/o/r/releases/tag/v1",
                "assets_url": "http://{addr}/repos/o/r/releases/7/assets",
                "upload_url": "http://{addr}/repos/o/r/releases/7/assets{{?name,label}}",
                "id": 7,
                "node_id": "RE_kwDO",
                "tag_name": "v1",
                "target_commitish": "master",
                "name": "v1",
                "body": null,
                "draft": false,
                "prerelease": false,
                "created_at": "2026-01-01T00:00:00Z",
                "published_at": null,
                "author": null,
                "assets": []
            }}"#
        );
        http("200 OK", &body)
    }

    fn asset_json(addr: SocketAddr, name: &str, size: u64, state: &str) -> String {
        format!(
            r#"{{
                "url": "http://{addr}/repos/o/r/releases/assets/99",
                "browser_download_url": "http://{addr}/o/r/releases/download/v1/{name}",
                "id": 99,
                "node_id": "RA_kwDO",
                "name": "{name}",
                "label": null,
                "state": "{state}",
                "content_type": "application/octet-stream",
                "size": {size},
                "download_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "uploader": null
            }}"#
        )
    }

    fn uploaded_asset_response(addr: SocketAddr, name: &str, size: u64) -> String {
        http("201 Created", &asset_json(addr, name, size, "uploaded"))
    }

    const BODY_401: &str =
        r#"{"message":"Bad credentials","documentation_url":"https://docs.github.com/rest"}"#;
    const BODY_404: &str = r#"{"message":"Not Found"}"#;
    const BODY_422_ALREADY_EXISTS: &str = r#"{"message":"Validation Failed","errors":[{"resource":"ReleaseAsset","code":"already_exists","field":"name"}]}"#;

    fn write_artifact(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write artifact fixture");
        p
    }

    async fn run_upload(
        addr: SocketAddr,
        path: &std::path::Path,
        replace_existing_artifacts: bool,
    ) -> Result<()> {
        run_upload_with_env(addr, path, replace_existing_artifacts, &EmptyEnv).await
    }

    /// Like [`run_upload`] but with a caller-supplied [`EnvSource`], so a test
    /// can drive `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS` through an injected
    /// [`MapEnvSource`](anodizer_core::MapEnvSource) instead of mutating the
    /// global process env (which races parallel tests).
    async fn run_upload_with_env(
        addr: SocketAddr,
        path: &std::path::Path,
        replace_existing_artifacts: bool,
        env_source: &dyn anodizer_core::EnvSource,
    ) -> Result<()> {
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        upload_release_asset(UploadAssetRequest {
            octo: &octo,
            owner: "o",
            repo: "r",
            release_id: 7,
            tag: "v1",
            path,
            file_name: "app.tar.gz",
            replace_existing_artifacts,
            policy: &policy,
            retry_after: None,
            token: "test-token",
            env_source,
        })
        .await
    }

    #[tokio::test]
    async fn transient_401_then_success_retries_and_succeeds() {
        // Attempt 1: GET release OK, POST upload -> 401 (the v0.9.0
        // incident's flake). Attempt 2: GET release OK, POST -> 201.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_artifact(tmp.path(), "app.tar.gz", b"bytes");
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            vec![
                release_json(addr),
                http("401 Unauthorized", BODY_401),
                release_json(addr),
                uploaded_asset_response(addr, "app.tar.gz", 5),
            ]
        });

        let result = run_upload(addr, &path, false).await;

        assert!(
            result.is_ok(),
            "401 must retry to success: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "expected 2 attempts x (GET release + POST upload)"
        );
    }

    #[tokio::test]
    async fn partial_asset_422_deletes_then_reuploads() {
        // Attempt 1 leaves a partial server-side (401 flake mid-upload);
        // attempt 2's POST is rejected 422 already_exists by the partial.
        // Recovery must: probe (list -> state "starter"), delete (list +
        // DELETE), then attempt 3 re-uploads to success — all WITHOUT
        // `replace_existing_artifacts`, because a partial is never
        // published content.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_artifact(tmp.path(), "app.tar.gz", b"full-content");
        let partial_list = |addr| {
            http(
                "200 OK",
                &format!("[{}]", asset_json(addr, "app.tar.gz", 4, "starter")),
            )
        };
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            vec![
                // attempt 1
                release_json(addr),
                http("401 Unauthorized", BODY_401),
                // attempt 2
                release_json(addr),
                http("422 Unprocessable Entity", BODY_422_ALREADY_EXISTS),
                // probe list (find_release_asset_probe)
                partial_list(addr),
                // delete: list + DELETE
                partial_list(addr),
                http("204 No Content", ""),
                // attempt 3
                release_json(addr),
                uploaded_asset_response(addr, "app.tar.gz", 12),
            ]
        });

        let result = run_upload(addr, &path, false).await;

        assert!(
            result.is_ok(),
            "partial-asset 422 must delete + re-upload to success: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            9,
            "expected 3 attempts (2 GET+POST pairs + 1 GET+POST) + probe list + delete list + DELETE"
        );
    }

    #[tokio::test]
    async fn published_asset_422_still_bails_when_replace_forbidden() {
        // A fully-uploaded asset with different bytes and
        // `replace_existing_artifacts: false` must keep failing fast —
        // the partial-asset recovery must NOT erode the immutable-assets
        // guarantee.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_artifact(tmp.path(), "app.tar.gz", b"different-content");
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            vec![
                release_json(addr),
                http("422 Unprocessable Entity", BODY_422_ALREADY_EXISTS),
                // probe list: published asset, size differs from local
                http(
                    "200 OK",
                    &format!("[{}]", asset_json(addr, "app.tar.gz", 4, "uploaded")),
                ),
            ]
        });

        let result = run_upload(addr, &path, false).await;

        let err = result.expect_err("differing published asset + replace=false must bail");
        assert!(
            format!("{err:#}").contains("replace_existing_artifacts"),
            "error must point at the replace_existing_artifacts knob: {err:#}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "bail must not delete or re-upload (1 GET + 1 POST + 1 probe list)"
        );
    }

    /// Secondary-rate-limit EXHAUSTION: every upload attempt's POST returns a
    /// 403 secondary-rate-limit response. The loop must back off between
    /// attempts (honoring the env-tuned delay), exhaust the bounded budget,
    /// and surface an actionable error that BOTH reports the attempt budget
    /// AND names the secondary rate limit (so an operator knows the cause is a
    /// burst guard, not a bare 403 auth failure or a transient 5xx).
    ///
    /// `test_retry_policy` resolves max_attempts=5 (> the MIN floor of 3), so
    /// 5 attempts x (GET release + POST 403) = 10 scripted responses are
    /// consumed before the loop gives up. `ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS=1`
    /// caps each inter-attempt sleep at ~1 s (×0.8–1.2 jitter) so the test
    /// proves the backoff fires without paying the real 60 s floor.
    #[tokio::test]
    async fn secondary_rate_limit_exhaustion_surfaces_actionable_error() {
        let body_403 = r#"{"message":"You have exceeded a secondary rate limit and have been temporarily blocked from content creation. Please retry your request again later.","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_artifact(tmp.path(), "app.tar.gz", b"bytes");
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            // 5 attempts: each is a GET release (200) then a POST upload (403
            // secondary-RL).
            let mut v = Vec::new();
            for _ in 0..5 {
                v.push(release_json(addr));
                v.push(http("403 Forbidden", body_403));
            }
            v
        });

        // Keep the per-attempt secondary-RL backoff at ~1 s instead of the
        // real 60 s floor so the test stays fast. Injected through a
        // `MapEnvSource` rather than the global process env so parallel tests
        // never race the override window.
        let env =
            anodizer_core::MapEnvSource::new().with("ANODIZER_GITHUB_SECONDARY_RL_DELAY_SECS", "1");
        let result = run_upload_with_env(addr, &path, false, &env).await;

        let err = result.expect_err("persistent secondary-RL 403 must fail after bounded attempts");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("failed after 5 attempts"),
            "error must report the exhausted attempt budget: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("secondary rate limit"),
            "error must name the secondary rate limit (actionable, not a bare 403): {rendered}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            10,
            "expected 5 attempts x (GET release + POST 403)"
        );
    }

    #[tokio::test]
    async fn permanent_404_fails_after_bounded_attempts() {
        // A genuinely missing release (404 on every GET) must exhaust the
        // bounded attempts and fail — not spin forever and not succeed.
        // `test_retry_policy` resolves max_attempts=5 (> the floor of 3),
        // so 5 GETs are consumed before the loop gives up.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_artifact(tmp.path(), "app.tar.gz", b"bytes");
        let (addr, calls) =
            spawn_oneshot_http_responder_with(|_addr| vec![http("404 Not Found", BODY_404); 5]);

        let result = run_upload(addr, &path, false).await;

        let err = result.expect_err("persistent 404 must fail after bounded attempts");
        assert!(
            format!("{err:#}").contains("failed after 5 attempts"),
            "error must report the exhausted attempt budget: {err:#}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "each attempt's GET /releases/{{id}} 404s before any POST"
        );
    }
}
