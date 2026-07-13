//! Retry classification and driver for `cargo publish` under transient
//! network and sparse-index propagation failures.

use super::*;

/// Heuristic: does this cargo-publish stderr look like it failed because
/// the sparse index hadn't caught up with a just-published dependency?
///
/// `poll_crates_io_index` already waits for the dep to appear on the edge
/// anodizer queries, but cargo's own publish invocation may hit a different
/// Fastly edge whose cache hasn't fanned out yet. The cargo error
/// signatures that show up in that race:
///
/// - `no matching package named '<crate>' found` — cargo couldn't locate
///   the dep at all in its registry view (the historical signature; see
///   the comment on `expand_with_transitive_deps`).
/// - `failed to select a version for the requirement '<crate> = "^X.Y.Z"'`
///   — cargo found the crate but not the just-published version; the
///   post-publish race window where cargo's resolution hits a stale
///   Fastly edge.
/// - `failed to load source for dependency '<crate>'` — sparse-index
///   transport error variant that cargo emits when the fetch itself fails
///   mid-resolution (less common but seen during Fastly fan-out windows).
///
/// All three are recoverable by waiting a few seconds and retrying. Any
/// other failure mode (auth, packaging, validation, network) does NOT
/// benefit from retry and is left to bubble up unchanged.
///
/// # Brittleness
///
/// These substrings are scraped from cargo's human-readable stderr. Cargo
/// does NOT guarantee stable error message wording across minor versions:
/// past Rust releases have renamed resolution-failure messages without a
/// deprecation period. If cargo restructures any of these strings the
/// discriminator silently stops firing, causing spurious publish failures
/// that look like hard errors instead of retryable propagation lag.
///
/// The unit tests below pin the cargo version against which these strings
/// were last verified (`cargo_version_matches_pinned_strings`). If CI
/// upgrades cargo to a different major.minor, that test fails and the
/// maintainer must re-verify the substrings against the new cargo output
/// before updating the pinned version. The strings were last verified
/// against **cargo 1.97.x** (verified in rust-lang/cargo branch
/// rust-1.97.0: resolver/errors.rs and core/registry.rs, 2026-07-10).
pub(crate) fn is_index_propagation_failure(stderr: &str) -> bool {
    stderr.contains("no matching package")
        || stderr.contains("failed to select a version")
        || stderr.contains("failed to load source for dependency")
}

/// Whether `cargo publish` failed with a transient network/transport fault a
/// retry can plausibly recover from.
///
/// Distinct from [`is_index_propagation_failure`] (sparse-index edge skew):
/// these are TCP/TLS/HTTP transport faults talking to crates.io or its CDN.
/// `cargo publish` makes live network calls mid-run — the registry index
/// update and the verification dep-download — and cargo retries "spurious"
/// transport errors a few times internally, but that budget is exhaustible.
/// A v0.11.3 release cut died with `[16] Error in the HTTP2 framing layer`
/// after cargo's own retries ran out, aborting the sequential workspace
/// publish 12 crates in and burning the re-cut attempt. An outer bounded
/// retry with backoff recovers from a momentary blip without masking real
/// failures: auth, packaging, and validation errors do NOT match here and
/// fast-fail unchanged on the first attempt.
///
/// Matched case-insensitively because curl and cargo vary the casing of the
/// same underlying fault across versions and platforms.
pub(crate) fn is_transient_network_failure(stderr: &str) -> bool {
    const NEEDLES: &[&str] = &[
        // libcurl transport faults surfaced verbatim by cargo's HTTP stack.
        "error in the http2 framing layer", // the v0.11.3 makeself failure
        "connection reset by peer",
        "connection refused",
        "could not resolve host",
        "couldn't resolve host",
        "resolving timed out",
        "operation timed out",
        "transfer closed",
        // cargo's own transport-layer wording. Deliberately NOT the bare
        // "failed to download": cargo emits it for a non-transient missing/
        // yanked dependency too (paired with "no matching package", which the
        // propagation class already handles), so matching it here would burn
        // retries on a hard resolution error. The curl faults above and the
        // request-send phrases below cover genuine transport download failures.
        "spurious network error",
        "failed to get successful http response",
        "failed to send http request",
        "error sending request",
        // 5xx / rate-limit from the registry or its CDN edge.
        "502 bad gateway",
        "503 service unavailable",
        "504 gateway timeout",
        "429 too many requests",
    ];
    let lower = stderr.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Run `cargo publish` with bounded retry on the two recoverable failure
/// classes: sparse-index propagation lag ([`is_index_propagation_failure`])
/// and transient network/transport faults ([`is_transient_network_failure`]).
///
/// This is defense-in-depth on top of [`poll_crates_io_index`] and cargo's
/// own internal transport retries. Even after our wait sees the just-published
/// dep on the crates.io sparse index, the dependent crate's own `cargo publish`
/// may race against Fastly's inter-edge fan-out and land on a stale edge; and a
/// momentary TCP/TLS/HTTP blip can exhaust cargo's bounded internal retries
/// mid-publish (the v0.11.3 `HTTP2 framing layer` abort). Retrying exclusively
/// on those two narrow signature sets recovers both windows without masking
/// real failures (auth, packaging, validation) — which match neither
/// discriminator and fast-fail on the first attempt.
///
/// `backoff` is the sleep between retry attempts. Production callers pass
/// [`PUBLISH_PROPAGATION_BACKOFF`]; tests pass a short `Duration` so the
/// retry path is exercised without incurring real wall-clock cost.
///
/// Returns the successful `Output` or bubbles the last failure verbatim.
/// Non-propagation failures fast-fail on the first attempt (no retry).
///
/// `registry_token`, when `Some`, is set as `CARGO_REGISTRY_TOKEN` on the
/// spawned child (a short-lived Trusted-Publishing token) — NEVER passed on
/// the argv, so it cannot leak into the process list. `None` spawns with the
/// ambient env unchanged, byte-identical to the historical token path.
pub(crate) fn run_cargo_publish_with_retry(
    cmd: &[String],
    label: &str,
    log: &StageLogger,
    backoff: std::time::Duration,
    registry_token: Option<&str>,
) -> Result<std::process::Output> {
    use anodizer_core::retry::{RetryLog, RetryStep, retry_steps_sync};

    // Terminal failure the shared step engine carries out of the ladder. A
    // spawn error bubbles verbatim; an exited process (fast-fail on a
    // non-recoverable class, or the last output after exhaustion) is finalized
    // through `check_output` exactly once — never on an intermediate retry,
    // whose error lines would otherwise spam the log for a failure that then
    // clears.
    enum PublishFailure {
        Spawn(anyhow::Error),
        Exited(std::process::Output),
    }

    let desc = format!("cargo publish for {label}");
    let res = retry_steps_sync(
        RetryLog::new(&desc, log),
        PUBLISH_PROPAGATION_RETRIES,
        None,
        |_attempt| -> RetryStep<std::process::Output, PublishFailure> {
            let mut command = Command::new(&cmd[0]);
            command.args(&cmd[1..]);
            // Trusted Publishing: inject the minted token via env only. Passing
            // it as `--token` on the argv would expose it in the process list.
            if let Some(tok) = registry_token {
                command.env("CARGO_REGISTRY_TOKEN", tok);
            }
            let output = match command.output() {
                Ok(o) => o,
                Err(e) => {
                    return RetryStep::Fail(PublishFailure::Spawn(
                        anyhow::Error::new(e)
                            .context(format!("publish: spawn `{}`", cmd.join(" "))),
                    ));
                }
            };
            if output.status.success() {
                return RetryStep::Done(output);
            }
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let propagation = is_index_propagation_failure(&stderr);
            let transient_net = is_transient_network_failure(&stderr);
            if !propagation && !transient_net {
                // Neither recoverable class — surface immediately (no retry).
                return RetryStep::Fail(PublishFailure::Exited(output));
            }
            // Name which recoverable class fired so the operator can tell an
            // edge-skew retry from a network-blip retry in the run log.
            let kind = if propagation {
                "sparse-index propagation lag"
            } else {
                "transient network error"
            };
            RetryStep::Retry {
                error: PublishFailure::Exited(output),
                delay: backoff,
                cause: kind.to_string(),
            }
        },
    );

    // Finalize every terminal path through `check_output` so the operator sees
    // the same redacted error envelope as the single-attempt path: success
    // returns `Ok`, an exited failure (fast-fail or post-exhaustion) formats to
    // `Err`, and a spawn error bubbles its own context.
    match res {
        Ok(output) => log.check_output(output, label),
        Err(PublishFailure::Exited(output)) => log.check_output(output, label),
        Err(PublishFailure::Spawn(e)) => Err(e),
    }
}
