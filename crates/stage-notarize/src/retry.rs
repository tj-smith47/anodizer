//! notarytool / rcodesign invocation with bounded retry on the documented
//! transient-failure output markers, plus the success/failure output check.

use std::process::Command;

use anyhow::{Result, bail};

const NOTARIZE_RETRY_ATTEMPTS: u32 = 3;
pub(super) const NOTARIZE_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

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
    initial_delay: std::time::Duration,
) -> Result<std::process::Output> {
    use anodizer_core::retry::{RetryLog, RetryStep, retry_steps_sync};
    debug_assert!(!args.is_empty(), "run_with_retry: empty args");

    let res = retry_steps_sync(
        RetryLog::new(label, log),
        NOTARIZE_RETRY_ATTEMPTS,
        None,
        |attempt| -> RetryStep<std::process::Output, NotarizeTerminal<std::process::Output>> {
            // `run_capture` routes through the shared capture loop so a
            // multi-minute `notarytool … --wait` earns the liveness heartbeat;
            // it returns the raw Output on any exit (for classification) and
            // Errs only on spawn/wait failure.
            match anodizer_core::run::run_capture(&mut build_command_from_args(args), log, label) {
                Ok(output) if output.status.success() => RetryStep::Done(output),
                Ok(output) if !is_retriable_notarize_output(&output, log) => {
                    // Non-retriable Apple-side rejection: terminal. Routed through
                    // `Fail` (not `Done`) so the engine never announces a rejected
                    // artifact as "succeeded"; the Output still reaches
                    // `check_notarize_output` for a coherent status message.
                    RetryStep::Fail(NotarizeTerminal::Exited(output))
                }
                Ok(output) => RetryStep::Retry {
                    error: NotarizeTerminal::Exited(output),
                    delay: notarize_backoff(initial_delay, attempt),
                    cause: "transient notarization error".to_string(),
                },
                Err(e) => RetryStep::Retry {
                    error: NotarizeTerminal::Spawn(
                        e.context(format!("notarize: failed to execute {label}")),
                    ),
                    delay: notarize_backoff(initial_delay, attempt),
                    cause: "spawn error".to_string(),
                },
            }
        },
    );
    match res {
        // Success, or an exhausted/non-retriable exit → hand the (possibly
        // failed) Output to the caller's reporting path; a spawn error carries
        // no Output, so bubble it as `Err`.
        Ok(output) | Err(NotarizeTerminal::Exited(output)) => Ok(output),
        Err(NotarizeTerminal::Spawn(err)) => Err(err),
    }
}

/// Terminal outcome the notarize retry engines carry out: a subprocess that ran
/// and exited (its `Output`/`ExitStatus` handed to the caller's reporting path)
/// vs. a spawn error that carries no result and must bubble as `Err`.
enum NotarizeTerminal<T> {
    Spawn(anyhow::Error),
    Exited(T),
}

/// Exponential backoff from `initial_delay` for notarize attempt `attempt`
/// (30s, 60s at the default 30s base). Saturating so a future widening of
/// `NOTARIZE_RETRY_ATTEMPTS` cannot overflow the shift.
fn notarize_backoff(initial_delay: std::time::Duration, attempt: u32) -> std::time::Duration {
    let mult = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
    initial_delay.saturating_mul(mult)
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
    initial_delay: std::time::Duration,
) -> Result<std::process::ExitStatus> {
    use anodizer_core::retry::{RetryLog, RetryStep, retry_steps_sync};
    debug_assert!(!args.is_empty(), "run_status_with_retry: empty args");

    let res = retry_steps_sync(
        RetryLog::new(label, log),
        NOTARIZE_RETRY_ATTEMPTS,
        None,
        |attempt| -> RetryStep<std::process::ExitStatus, NotarizeTerminal<std::process::ExitStatus>> {
            // `run_capture` pipes both streams (rcodesign's chatter never leaks
            // to the default log — surfaced only at `-v`), earns the liveness
            // heartbeat, and returns the raw Output; take just the exit status.
            match anodizer_core::run::run_capture(&mut build_command_from_args(args), log, label) {
                Ok(output) if output.status.success() => RetryStep::Done(output.status),
                Ok(output) => RetryStep::Retry {
                    error: NotarizeTerminal::Exited(output.status),
                    delay: notarize_backoff(initial_delay, attempt),
                    cause: "non-success exit".to_string(),
                },
                Err(e) => RetryStep::Retry {
                    error: NotarizeTerminal::Spawn(
                        e.context(format!("notarize: failed to execute {label}")),
                    ),
                    delay: notarize_backoff(initial_delay, attempt),
                    cause: "spawn error".to_string(),
                },
            }
        },
    );
    match res {
        // Success, or an exhausted non-success exit → return the last status for
        // the caller to report; a spawn error carries no status, so bubble it.
        Ok(status) | Err(NotarizeTerminal::Exited(status)) => Ok(status),
        Err(NotarizeTerminal::Spawn(err)) => Err(err),
    }
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
        // Defense-in-depth: a zero exit is the documented accepted signal, but
        // never trust it past a body that explicitly says invalid/rejected.
        // Treat exit-0-with-rejection as a failure rather than stapling an
        // un-notarized artifact.
        if combined_lower.contains("status: invalid")
            || combined_lower.contains("invalid submission")
        {
            bail!(
                "notarize: {}: exited 0 but reported status invalid — the submitted artifact did not pass Apple's checks",
                label
            );
        }
        if combined_lower.contains("status: rejected")
            || combined_lower.contains("submission rejected")
        {
            bail!("notarize: {}: exited 0 but reported status rejected", label);
        }
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
