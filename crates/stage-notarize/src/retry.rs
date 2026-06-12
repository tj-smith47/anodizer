//! notarytool / rcodesign invocation with bounded retry on the documented
//! transient-failure output markers, plus the success/failure output check.

use std::process::Command;

use anyhow::{Result, bail};

const NOTARIZE_RETRY_ATTEMPTS: u32 = 3;
const NOTARIZE_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// Substrings (lowercased) on stderr/stdout that signal a transient
/// network-side failure rather than a real "this artifact is invalid"
/// rejection by Apple. Anything outside this set is a non-retriable failure
/// (`status: invalid`, `status: rejected`, malformed args, file not found,
/// etc.) and bypasses the retry loop.
const RETRIABLE_OUTPUT_MARKERS: &[&str] = &[
    "connection",
    "connect: ",
    "timeout",
    "timed out",
    "tls",
    "ssl",
    "i/o",
    "could not resolve",
    "name resolution",
    "temporary failure",
    "503",
    "504",
    "502",
    "429",
    "service unavailable",
    "gateway",
    "dial tcp",
    "broken pipe",
    "reset by peer",
    "eof",
    "unable to connect",
    "network is unreachable",
];

/// True when the combined stderr/stdout suggests the failure is transient
/// (network blip, retriable HTTP status). Apple-side hard rejections must
/// not retry; treat them as terminal so misconfigured artifacts fail fast
/// instead of burning multi-minute App Store Connect API quota.
///
/// Output is run through the logger's env-driven redaction BEFORE the
/// substring contains-check so a credential value that happens to coincide
/// with a retry marker substring cannot influence retry classification —
/// and so any diagnostic log written from inside the retry loop reads from
/// the same scrubbed text.
pub(super) fn is_retriable_notarize_output(
    output: &std::process::Output,
    log: &anodizer_core::log::StageLogger,
) -> bool {
    let raw = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = log.redact(&raw).to_lowercase();
    if combined.contains("status: invalid")
        || combined.contains("invalid submission")
        || combined.contains("status: rejected")
        || combined.contains("submission rejected")
    {
        return false;
    }
    RETRIABLE_OUTPUT_MARKERS
        .iter()
        .any(|marker| combined.contains(marker))
}

/// Build a Command from a `[bin, arg, arg, ...]` slice — used by the retry
/// helper because `Command` is not `Clone`-able and we need to re-execute
/// the same invocation on each attempt.
pub(super) fn build_command_from_args(args: &[String]) -> Command {
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd
}

/// Run a command up to `NOTARIZE_RETRY_ATTEMPTS` times, sleeping
/// exponentially between attempts (30s, 60s). Retries on:
///   1. spawn error (could not execute the binary — network filesystem, etc.),
///   2. non-zero exit whose combined stdout+stderr matches a transient marker.
///
/// On the final attempt the result (success OR non-retriable failure) is
/// returned to the caller without further sleeping. `label` is used for
/// human-readable retry diagnostics; `delay_fn` lets tests pass a no-op
/// sleeper so the suite cannot accidentally wait 30 seconds.
pub(super) fn run_with_retry(
    args: &[String],
    label: &str,
    log: &anodizer_core::log::StageLogger,
    delay_fn: &dyn Fn(std::time::Duration),
) -> Result<std::process::Output> {
    debug_assert!(!args.is_empty(), "run_with_retry: empty args");
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..NOTARIZE_RETRY_ATTEMPTS {
        let try_n = attempt + 1;
        match build_command_from_args(args).output() {
            Ok(output) => {
                if output.status.success() || !is_retriable_notarize_output(&output, log) {
                    return Ok(output);
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                last_err = Some(anyhow::anyhow!(
                    "notarize: {} attempt {}/{} failed transiently (exit {:?}): {}",
                    label,
                    try_n,
                    NOTARIZE_RETRY_ATTEMPTS,
                    output.status.code(),
                    stderr.trim(),
                ));
                if try_n == NOTARIZE_RETRY_ATTEMPTS {
                    // Final attempt — return the (failed) output so the
                    // existing `check_notarize_output` reporting path
                    // produces a coherent error message.
                    return Ok(output);
                }
            }
            Err(e) => {
                last_err = Some(anyhow::Error::new(e).context(format!(
                    "notarize: failed to execute {} (attempt {}/{})",
                    label, try_n, NOTARIZE_RETRY_ATTEMPTS,
                )));
                if try_n == NOTARIZE_RETRY_ATTEMPTS {
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow::anyhow!(
                            "notarize: {} failed after {} attempts",
                            label,
                            NOTARIZE_RETRY_ATTEMPTS
                        )
                    }));
                }
            }
        }
        // Exponential backoff: 30s, 60s.
        let delay = NOTARIZE_INITIAL_DELAY * 2u32.pow(attempt);
        log.warn(&format!(
            "{} attempt {}/{} hit a transient error; retrying in {}s",
            label,
            try_n,
            NOTARIZE_RETRY_ATTEMPTS,
            delay.as_secs(),
        ));
        delay_fn(delay);
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "notarize: {} exhausted {} attempts without a result",
            label,
            NOTARIZE_RETRY_ATTEMPTS
        )
    }))
}

/// Default delay function for the retry helpers (real `thread::sleep`).
pub(super) fn real_sleep(d: std::time::Duration) {
    std::thread::sleep(d);
}

/// Variant of `run_with_retry` for callers that only need the exit status
/// (`.status()` style). Used by `rcodesign sign` which contacts Apple's
/// RFC 3161 timestamp server (`timestamp.apple.com`) and so is itself a
/// network-touching call. Without `.output()` we cannot inspect stderr to
/// classify failure as transient vs. permanent, so this variant retries on
/// **any** non-success exit (and on spawn errors). The exponential schedule
/// matches `run_with_retry`. Callers that have output-classification fidelity
/// should prefer `run_with_retry`.
pub(super) fn run_status_with_retry(
    args: &[String],
    label: &str,
    log: &anodizer_core::log::StageLogger,
    delay_fn: &dyn Fn(std::time::Duration),
) -> Result<std::process::ExitStatus> {
    debug_assert!(!args.is_empty(), "run_status_with_retry: empty args");
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..NOTARIZE_RETRY_ATTEMPTS {
        let try_n = attempt + 1;
        match build_command_from_args(args).status() {
            Ok(status) if status.success() => return Ok(status),
            Ok(status) => {
                last_err = Some(anyhow::anyhow!(
                    "notarize: {} attempt {}/{} exited {:?}",
                    label,
                    try_n,
                    NOTARIZE_RETRY_ATTEMPTS,
                    status.code(),
                ));
                if try_n == NOTARIZE_RETRY_ATTEMPTS {
                    return Ok(status);
                }
            }
            Err(e) => {
                last_err = Some(anyhow::Error::new(e).context(format!(
                    "notarize: failed to execute {} (attempt {}/{})",
                    label, try_n, NOTARIZE_RETRY_ATTEMPTS,
                )));
                if try_n == NOTARIZE_RETRY_ATTEMPTS {
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow::anyhow!(
                            "notarize: {} failed after {} attempts",
                            label,
                            NOTARIZE_RETRY_ATTEMPTS
                        )
                    }));
                }
            }
        }
        let delay = NOTARIZE_INITIAL_DELAY * 2u32.pow(attempt);
        log.warn(&format!(
            "{} attempt {}/{} failed; retrying in {}s",
            label,
            try_n,
            NOTARIZE_RETRY_ATTEMPTS,
            delay.as_secs(),
        ));
        delay_fn(delay);
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "notarize: {} exhausted {} attempts without a result",
            label,
            NOTARIZE_RETRY_ATTEMPTS
        )
    }))
}

// ---------------------------------------------------------------------------
// Helper: parse notarize output for status differentiation
// ---------------------------------------------------------------------------

/// Check notarization subprocess output, differentiating between rejected,
/// invalid, timeout, and accepted statuses (the
/// differentiates AcceptedStatus, InvalidStatus, RejectedStatus, TimeoutStatus).
pub(super) fn check_notarize_output(
    output: &std::process::Output,
    label: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);
    let combined_lower = combined.to_lowercase();

    if output.status.success() {
        // Even on success, check for status keywords to provide accurate logging
        if combined_lower.contains("status: accepted") || combined_lower.contains("status: success")
        {
            log.status(&format!("{} succeeded (accepted)", label));
        } else if combined_lower.contains("timeout") {
            // timeout is non-fatal (logs info, no error)
            log.warn(&format!(
                "{} timed out (submission may still be processing)",
                label
            ));
        }
        return Ok(());
    }

    // Non-zero exit: differentiate error type from output
    if combined_lower.contains("status: invalid") || combined_lower.contains("invalid submission") {
        bail!(
            "notarize: {}: invalid — the submitted artifact did not pass Apple's checks",
            label
        );
    }
    if combined_lower.contains("status: rejected") || combined_lower.contains("submission rejected")
    {
        bail!(
            "notarize: {}: rejected — Apple rejected the notarization request",
            label
        );
    }
    if combined_lower.contains("timeout") || combined_lower.contains("timed out") {
        // timeout is non-fatal (info log, not error)
        log.warn(&format!(
            "{} timed out waiting for Apple response (submission may still be processing)",
            label
        ));
        return Ok(());
    }

    // Generic failure
    bail!(
        "notarize: {} failed (exit code: {:?})\n{}",
        label,
        output.status.code(),
        combined.trim()
    );
}

// ---------------------------------------------------------------------------
// NotarizeStage
