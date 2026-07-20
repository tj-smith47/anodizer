//! Shared sign processing — the core driver behind both `signs:` (normal
//! artifact signing) and `binary_signs:` (per-binary signing). Owns the
//! `SignJob` value type, the `process_sign_configs` driver, and the
//! parallel-execution wrapper.
//!
//! Split out from `lib.rs` so the per-job flow (filter → render → execute)
//! is independently reviewable without scrolling through SignStage glue.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodizer_core::log::StageLogger;

mod authenticode;
mod cosign;
mod sign_configs;

#[cfg(test)]
mod tests;

pub(crate) use authenticode::*;
pub(crate) use cosign::*;
pub(crate) use sign_configs::*;

/// Artifact filter mode for `process_sign_configs`.
#[derive(Clone, Copy)]
pub(crate) enum ArtifactFilter {
    /// Use the `artifacts` field from each SignConfig (or default to "none").
    FromConfig,
    /// Always restrict to `ArtifactKind::Binary`, regardless of config.
    BinaryOnly,
    /// Re-sign ONLY the combined `checksums.txt` files, using each
    /// SignConfig's own `artifacts` filter (so only a config that signs
    /// checksums participates). Used by [`crate::resign_combined_checksums`]
    /// after the release stage rewrites `checksums.txt` to fold in
    /// publish-time artifacts, which would otherwise leave the earlier
    /// signature stale. Any stale `.sig`/`.pem` sidecars are removed before
    /// signing so the reused signer writes a fresh signature over the
    /// refreshed bytes (the default `gpg --output` refuses to overwrite a
    /// file already on disk non-interactively).
    CombinedChecksumOnly,
}

/// A fully-prepared sign job ready for parallel execution.
///
/// All template rendering and path resolution is done up-front so that the
/// actual signing command can be spawned without borrowing the `Context`.
struct SignJob {
    /// The signing command binary (e.g., "gpg", "cosign").
    cmd: String,
    /// Fully-resolved command arguments.
    args: Vec<String>,
    /// Optional stdin content to pipe to the signing command.
    stdin_data: Option<Vec<u8>>,
    /// Optional environment variables to set on the child process, ordered.
    env: Option<Vec<(String, String)>>,
    /// Extra secret values to scrub from the child's stdout/stderr regardless
    /// of whether they are exported as child env. Each entry is a
    /// `(synthetic_key, value)` pair fed to [`anodizer_core::redact::string`];
    /// the key only governs the masked replacement spelling, so it is chosen to
    /// always trip `is_secret` (e.g. a `*_PASSWORD` suffix). The Authenticode
    /// path uses this for the cert password — which is passed in argv, never
    /// deliberately exported to the child env — so a tool echoing it on error
    /// is still masked even when the user's `password_env` key carries no
    /// secret suffix.
    redact_extra: Vec<(String, String)>,
    /// Env var names to strip from the child's *inherited* environment before
    /// spawning. `Command` does not `env_clear`, so the child would otherwise
    /// inherit the whole parent env. The Authenticode path lists its
    /// `password_env` here so the cert password reaches the signer only via
    /// argv (`-pass`/`/p`) and never as an inherited env var a misbehaving
    /// tool could dump. Empty for every other job.
    env_remove: Vec<String>,
    /// Human-readable label for log messages (e.g., "sign", "binary-sign").
    label: String,
    /// The sign config's `id` field for log messages.
    id_label: String,
    /// Display string for the artifact being signed (used in log messages).
    artifact_display: String,
    /// Display string for the signature output path (used in log messages).
    signature_display: String,
    /// Whether to capture and log the command's stdout/stderr.
    output_flag: bool,
    /// Artifact registrations to add after signing (signature + optional certificate).
    new_artifacts: Vec<anodizer_core::artifact::Artifact>,
    /// `(from, to)` atomic rename applied after the signer exits 0.
    ///
    /// osslsigncode requires a distinct `-out` path, so the Authenticode job
    /// signs to a sibling temp (`from`) and then renames it over the original
    /// artifact (`to`). `None` for every detached (cosign/gpg) job and for the
    /// in-place signtool path.
    rename_after: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// When set, the per-artifact RESULT line emitted at status level after a
    /// successful Authenticode sign (e.g. `authenticode-signed myapp.exe`). The
    /// detached path leaves this `None` (its result is the registered `.sig`).
    authenticode_result: Option<String>,
    /// Post-sign verification command, executed right after the sign
    /// succeeds so "the signer exited 0" is upgraded to "the signature
    /// verifies". `None` when verification is disabled, skipped (inputs not
    /// derivable), or in dry-run.
    verify: Option<crate::verify::VerifyJob>,
}

/// Best-effort removal of an Authenticode job's `-out` temp on the error path.
///
/// The osslsigncode path signs to a sibling temp (`rename_after.0`) and only
/// renames it over the original on success. Any failure before the rename
/// (spawn, wait, or a non-zero signer exit) must clean up the partial temp so
/// no `.authenticode-tmp` litter file is left behind. No-op for the detached
/// (cosign/gpg) and in-place signtool paths, which carry no `rename_after`.
fn cleanup_rename_temp(job: &SignJob) {
    if let Some((from, _)) = &job.rename_after {
        let _ = std::fs::remove_file(from);
    }
}

/// Execute a single prepared sign job, returning `Ok(())` on success.
fn execute_sign_job(job: &SignJob, log: &StageLogger) -> Result<()> {
    // Per-artifact detail — at default verbosity the `signing N artifacts`
    // summary (emitted once before this loop) is the status-level signal; the
    // per-artifact `sign X → Y` line would flood the log on wide fan-outs.
    log.verbose(&format!(
        "signing {} → {} ({}[{}])",
        job.artifact_display, job.signature_display, job.label, job.id_label
    ));

    let stdin_cfg = if job.stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::inherit()
    };

    let mut command = Command::new(&job.cmd);
    command
        .args(&job.args)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ref env_vars) = job.env {
        for (k, v) in env_vars {
            command.env(k, v);
        }
    }
    // Strip inherited secret env vars (e.g. the Authenticode `password_env`) so
    // the secret reaches the signer only via argv, not as a child env var.
    for k in &job.env_remove {
        command.env_remove(k);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to spawn '{}' for {}",
                    job.label, job.cmd, job.artifact_display
                )
            });
        }
    };

    if let Some(ref data) = job.stdin_data {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(data).with_context(|| {
                format!(
                    "{}: failed to write stdin for {}",
                    job.label, job.artifact_display
                )
            })?;
            drop(child_stdin); // Explicitly close stdin so child sees EOF
        } else {
            // Proceeding would run the signer WITHOUT its intended stdin,
            // producing a signature over missing input. Fail hard instead.
            cleanup_rename_temp(job);
            anyhow::bail!(
                "{}: stdin data was provided but the child process stdin is \
                 unavailable for {} — refusing to sign without it",
                job.label,
                job.artifact_display
            );
        }
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to wait for '{}' for {}",
                    job.label, job.cmd, job.artifact_display
                )
            });
        }
    };

    // Redact secrets from stdout/stderr before any output or logging.
    // The scrub set is the child env PLUS `redact_extra` (secrets passed via
    // argv, e.g. the Authenticode cert password, which the child env never
    // carries) PLUS the process environment. `redact::string` masks each entry
    // whose key trips `is_secret`; `redact_extra` keys are chosen to always
    // trip it, so the value is masked regardless of the user's env-var name.
    let env_pairs: Vec<(String, String)> = job
        .env
        .iter()
        .flat_map(|m| m.iter().cloned())
        .chain(job.redact_extra.iter().cloned())
        .chain(std::env::vars())
        .collect();

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

    let stdout_str = anodizer_core::redact::string(&stdout_raw, &env_pairs);
    let stderr_str = anodizer_core::redact::string(&stderr_raw, &env_pairs);

    // Raw subprocess stdio is verbose-only detail per
    // .claude/rules/log-status-vs-verbose.md; an explicit `output:` opts the
    // tee back in but it stays below default. A non-zero exit still surfaces
    // via `check_output` below.
    if job.output_flag {
        if !stdout_str.is_empty() {
            log.verbose(&format!("[{} stdout] {}", job.label, stdout_str.trim()));
        }
        if !stderr_str.is_empty() {
            log.verbose(&format!("[{} stderr] {}", job.label, stderr_str.trim()));
        }
    }

    let mut redacted_output = output;
    redacted_output.stdout = stdout_str.into_bytes();
    redacted_output.stderr = stderr_str.into_bytes();

    if let Err(e) = log.check_output(redacted_output, &job.cmd) {
        // A non-zero signer exit may have left a partial `-out` temp; remove it
        // so a failed Authenticode sign leaves neither a clobbered original nor
        // a `.authenticode-tmp` litter file behind.
        cleanup_rename_temp(job);
        return Err(e);
    }

    // Authenticode (osslsigncode) writes to a sibling temp; atomically replace
    // the original artifact only after the signer succeeded so a failed sign
    // never leaves a half-written file in place.
    if let Some((from, to)) = &job.rename_after {
        if let Err(e) = std::fs::rename(from, to) {
            // A failed rename (e.g. cross-device, permissions) would otherwise
            // strand the signed temp next to the untouched original; remove it
            // so the error path leaves no `.authenticode-tmp` litter behind.
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to move signed temp {} over {}",
                    job.label,
                    from.display(),
                    to.display()
                )
            });
        }
    }

    if let Some(result) = &job.authenticode_result {
        log.status(result); // status-ok: per-artifact authenticode result line
    }
    Ok(())
}

/// Retry policy for cosign invocations.
///
/// cosign talks to sigstore infrastructure (Fulcio, Rekor, the TUF CDN), so a
/// non-zero exit is frequently transient. The canonical failure is the cold
/// TUF-cache flock race on a fresh CI runner (`creating cached local store:
/// resource temporarily unavailable`), whose contention window spans multiple
/// seconds — cosign's own 3 internal tries burn out in ~2.5s, entirely inside
/// it. The nominal spread here is 2+4+8+15 = 29s before the last attempt,
/// wide enough to outlive the window even under jitter shrink.
pub(crate) const COSIGN_TRANSIENT_RETRY: anodizer_core::retry::RetryPolicy =
    anodizer_core::retry::RetryPolicy {
        max_attempts: 5,
        base_delay: std::time::Duration::from_secs(2),
        max_delay: std::time::Duration::from_secs(15),
    };

/// Retry `op` per `policy` with jittered exponential backoff, routed through
/// the shared step engine ([`anodizer_core::retry::retry_steps_sync`]).
///
/// The engine owns the attempt cap, the per-attempt warn, and the backoff sleep
/// (with the run's retry accounting); this wrapper owns only the classification
/// and the jittered delay. The jitter is caller-owned via [`sign_retry_delay`]
/// because concurrent sign workers retrying a shared-lock collision must
/// de-synchronize or they re-collide every round — a property a fixed policy
/// delay cannot express.
///
/// A spawn failure with `ErrorKind::NotFound` fast-fails: a missing signer
/// binary cannot heal, and burning the full backoff budget on it would turn a
/// one-line config error into a half-minute stall. Deterministic signer
/// failures ([`is_deterministic_sign_failure`]) fast-fail for the same
/// reason: a flag typo, an unparseable key, or a certificate-identity
/// mismatch is identical on attempt 5, and the full ladder burns ~29s per
/// artifact. Anything unmatched keeps retrying (fail-open): the retry
/// exists for the ambiguous network/TUF/flock class, and mis-classifying a
/// transient failure as deterministic would break signing outright, while
/// mis-classifying a deterministic failure as transient only costs time.
pub(crate) fn retry_transient(
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
    what: &str,
    op: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    use anodizer_core::retry::{RetryLog, RetryStep, retry_steps_sync};
    let desc = format!("sign {what}");
    retry_steps_sync(
        RetryLog::new(&desc, log),
        policy.max_attempts,
        None,
        |attempt| match op() {
            Ok(()) => RetryStep::Done(()),
            Err(e) => {
                let unspawnable = e
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                if unspawnable || is_deterministic_sign_failure(&e) {
                    RetryStep::Fail(e)
                } else {
                    let cause = e.root_cause().to_string();
                    RetryStep::Retry {
                        error: e,
                        delay: sign_retry_delay(policy, attempt + 1),
                        cause,
                    }
                }
            }
        },
    )
}

/// Jittered backoff before sign attempt `next_attempt`: the policy's
/// exponential delay spread ±20% by [`anodizer_core::retry::jitter_duration`].
///
/// A pure helper so the schedule's contention-window envelope (each delay
/// within ±20% of nominal, nominal spread ≥15s to outlive the cold-TUF flock
/// race) is testable without driving the multi-second retry loop.
pub(crate) fn sign_retry_delay(
    policy: &anodizer_core::retry::RetryPolicy,
    next_attempt: u32,
) -> std::time::Duration {
    anodizer_core::retry::jitter_duration(policy.delay_for(next_attempt))
}

/// True when a signer failure is deterministic — re-running the identical
/// command must produce the identical failure — so retrying only burns the
/// backoff budget.
///
/// Matches against the full error chain (`{:#}`), which carries the signer's
/// captured stderr via `StageLogger::check_output`, lowercased. The needle
/// classes, with the cosign stderr they pin:
///
/// * CLI usage errors — cosign/gpg flag or subcommand typos
///   (`unknown flag: --keyy`, `unknown command "sing" for "cosign"`,
///   `flag needs an argument`, `accepts at most 1 arg(s)`).
/// * key/credential material that cannot parse —
///   `unsupported pem type`, `parsing private key`.
/// * verification-policy mismatches — `none of the expected identities
///   matched what was in the certificate` (cosign's certificate-identity
///   check).
///
/// Deliberately NOT matched: TUF/flock/Fulcio/Rekor/network failures
/// (`resource temporarily unavailable`, `creating cached local store`,
/// timeouts) — the retry ladder exists precisely for those, and the
/// unmatched default is retry, so new transient phrasings stay safe.
/// `no such file or directory` is also NOT matched: cosign surfaces the
/// same ENOENT phrasing for a cold or racing `~/.sigstore` TUF-cache read
/// (`open ~/.sigstore/...: no such file or directory`), which heals on
/// retry — a substring match would fast-fail the exact race the ladder
/// exists for. A genuinely-missing key/artifact path only costs the
/// backoff budget, never correctness.
pub(crate) fn is_deterministic_sign_failure(err: &anyhow::Error) -> bool {
    const DETERMINISTIC_NEEDLES: &[&str] = &[
        "unknown flag",
        "unknown command",
        "unknown shorthand flag",
        "flag needs an argument",
        "accepts at most",
        "accepts 1 arg(s)",
        "required flag(s)",
        "unsupported pem type",
        "parsing private key",
        "none of the expected identities matched",
        // Post-sign verification failures: a signature that does not match
        // its artifact/key is identical on every re-run (cosign's
        // "invalid signature when validating ASN.1 encoded signature",
        // gpg's "BAD signature from ...").
        "invalid signature",
        "bad signature",
    ];
    let chain = format!("{err:#}").to_lowercase();
    DETERMINISTIC_NEEDLES.iter().any(|n| chain.contains(n))
}

/// Convert a runtime label string to a `&'static str` for `StageLogger::new`.
fn label_to_static(label: &str) -> &'static str {
    match label {
        "sign" => "sign",
        "binary-sign" => "binary-sign",
        _ => "sign",
    }
}
