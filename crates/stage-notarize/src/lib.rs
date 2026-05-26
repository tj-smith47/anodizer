use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{MacOSNativeSignNotarizeConfig, MacOSSignNotarizeConfig, StringOrBool};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// Helper: refresh artifact checksums after signing
// ---------------------------------------------------------------------------

/// Re-compute SHA256 for all darwin artifacts whose bytes may have been
/// rewritten by the signing / stapling steps. Covers:
///
/// - `Binary` / `UniversalBinary` — cross-platform `rcodesign sign`
///   mutates the Mach-O in place.
/// - `DiskImage` — native `xcrun stapler staple` rewrites the DMG with
///   an embedded notarization ticket.
/// - `MacOsPackage` — `productsign` produces a freshly-signed `.pkg`
///   whose bytes differ from the unsigned input.
///
/// Skipping the DMG / PKG refresh would leave `metadata["sha256"]`
/// pointing at the pre-sign bytes, so any downstream publisher
/// (Homebrew cask, GitHub Release blob) would advertise a checksum that
/// fails on `brew install` / `shasum -a 256 -c`.
fn refresh_artifact_checksums(ctx: &mut Context, log: &anodizer_core::log::StageLogger) {
    for artifact in ctx.artifacts.all_mut() {
        if !matches!(
            artifact.kind,
            ArtifactKind::Binary
                | ArtifactKind::UniversalBinary
                | ArtifactKind::DiskImage
                | ArtifactKind::MacOsPackage
        ) {
            continue;
        }
        let is_darwin = artifact
            .target
            .as_deref()
            .map(anodizer_core::target::is_darwin)
            .unwrap_or(false);
        if !is_darwin {
            continue;
        }
        // Only refresh if sha256 metadata was previously set
        if !artifact.metadata.contains_key("sha256") {
            continue;
        }
        match anodizer_core::hashing::sha256_file(&artifact.path) {
            Ok(new_sha) => {
                artifact.metadata.insert("sha256".to_string(), new_sha);
            }
            Err(e) => {
                log.warn(&format!(
                    "notarize: failed to refresh sha256 for {}: {}",
                    artifact.path.display(),
                    e
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: check if a StringOrBool-typed `enabled` field is active
// ---------------------------------------------------------------------------

/// Returns `true` when the notarize entry should run, i.e. `skip:` is absent
/// or evaluates to false. Per-config notarize gating uses the canonical
/// `skip:` field, shared with every other publisher / pipe.
///
/// - `None` → run (default opt-in once a notarize block is present)
/// - `Some(Bool(false))` → run
/// - `Some(Bool(true))` → skip
/// - `Some(String(tmpl))` → render template, skip if result is "true"
fn is_active(skip: &Option<StringOrBool>, ctx: &Context) -> bool {
    let skipped = match skip {
        None => false,
        Some(sob) => {
            if sob.is_template() {
                ctx.render_template(sob.as_str())
                    .map(|r| r.trim() == "true")
                    .unwrap_or(false)
            } else {
                sob.as_bool()
            }
        }
    };
    !skipped
}

// ---------------------------------------------------------------------------
// Helper: render an optional template field
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helper: filter artifacts by ids list
// ---------------------------------------------------------------------------

use anodizer_core::artifact::matches_id_filter;

/// Check whether an artifact matches the given ids filter — delegates to the
/// canonical `anodizer_core::artifact::matches_id_filter` (GoReleaser `ByID`).
fn matches_ids(artifact: &Artifact, ids: &Option<Vec<String>>) -> bool {
    matches_id_filter(artifact, ids.as_deref())
}

// ---------------------------------------------------------------------------
// Base64 cert / key materialization
// ---------------------------------------------------------------------------

/// Detect whether `value` looks like a base64-encoded P12 / P8 blob
/// (as opposed to a filesystem path). GR's `notarize.macos[*].sign.certificate`
/// and `notarize.macos[*].notarize.key` both accept either spelling so the
/// secret can flow through an env-var without a sidecar file. The heuristic:
///
/// 1. Value is non-empty.
/// 2. Value does NOT contain a path separator (`/` or `\`).
/// 3. Value is longer than typical bare filenames (>= 64 chars) so we
///    don't false-positive on a literal `cert.p12`.
/// 4. Value matches the base64 alphabet (`[A-Za-z0-9+/=]`) end-to-end.
fn looks_like_base64(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains('/') || value.contains('\\') {
        return false;
    }
    if value.len() < 64 {
        return false;
    }
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'=' || b == b'\n' || b == b'\r')
}

/// Materialize a path-or-base64 value into a path the underlying tool
/// (rcodesign) can read directly. Returns either the original path
/// (string-cloned) or a [`tempfile::NamedTempFile`] that owns the
/// decoded bytes for the lifetime of the caller. The caller must keep
/// the [`MaterializedSecret`] alive until after the subprocess exits.
struct MaterializedSecret {
    /// Path string the caller passes to the subprocess.
    path: String,
    /// `Some` when we wrote a tempfile; `None` when we passed the user
    /// path through verbatim. Dropped at the end of the caller's scope
    /// to remove the on-disk decode.
    _tempfile: Option<tempfile::NamedTempFile>,
}

fn materialize_secret(value: &str, label: &str) -> Result<MaterializedSecret> {
    if !looks_like_base64(value) {
        return Ok(MaterializedSecret {
            path: value.to_string(),
            _tempfile: None,
        });
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.trim().replace(['\r', '\n'], "").as_bytes())
        .with_context(|| format!("notarize: base64-decode {}", label))?;
    let mut tf = tempfile::NamedTempFile::new()
        .with_context(|| format!("notarize: create tempfile for {}", label))?;
    {
        use std::io::Write as _;
        tf.write_all(&bytes)
            .with_context(|| format!("notarize: write decoded bytes to tempfile for {}", label))?;
    }
    let path = tf
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("notarize: tempfile path is not valid UTF-8 for {}", label))?
        .to_string();
    Ok(MaterializedSecret {
        path,
        _tempfile: Some(tf),
    })
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

// Default values are owned by the config impls (lazy-defaults policy).
// See `MacOSSignConfig::DEFAULT_TIMESTAMP_URL`,
// `MacOSNotarizeApiConfig::DEFAULT_TIMEOUT`, and
// `MacOSNativeNotarizeConfig::DEFAULT_TIMEOUT`.

// ---------------------------------------------------------------------------
// Helper: redact sensitive values from command args for safe logging
// ---------------------------------------------------------------------------

/// Redact sensitive values from command args for safe logging.
///
/// Two-pass redaction:
///   1. Per-flag pass — anything immediately following a known sensitive
///      flag (`--password`, `--api-key-path`, …) is swapped for `[REDACTED]`.
///      Catches the case where the credential is a literal CLI arg.
///   2. Env-value pass — the joined argv string is re-run through the
///      logger's env-driven redactor (`core::redact::string`), so secrets
///      that arrive via the environment (APPLE_API_KEY, APPLE_API_ISSUER,
///      AC_PASSWORD, …) but get echoed into argv via shell expansion are
///      replaced with `$KEY_NAME`. Tokens are only redacted if `log` has
///      env pairs attached; the per-flag pass still runs.
fn redact_args(args: &[String], log: &anodizer_core::log::StageLogger) -> Vec<String> {
    let sensitive_flags = [
        "--p12-password",
        "--api-key-path",
        "--api-key",
        "--password",
        "--token",
        "--apple-id-password",
    ];
    let mut result = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            result.push("[REDACTED]".to_string());
            redact_next = false;
        } else if sensitive_flags.iter().any(|f| arg == *f) {
            result.push(arg.clone());
            redact_next = true;
        } else {
            result.push(log.redact(arg));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// M6: retry policy for the network-touching subprocess calls in notarize
// ---------------------------------------------------------------------------
//
// The notarize stage shells out to Apple-hosted services (rcodesign
// notary-submit and xcrun notarytool submit); a transient blip
// (TLS handshake fail, 5xx, DNS hiccup, the well-known *AppleID*
// authentication 503s) used to fail the whole release. A top-level
// `retries:` config is being added in a separate wave; until then this
// stage carries a self-contained 3-attempt exponential schedule (delays
// 30s / 60s before the 2nd and 3rd attempts respectively).

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
fn is_retriable_notarize_output(
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
fn build_command_from_args(args: &[String]) -> Command {
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
fn run_with_retry(
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
            "notarize: {} attempt {}/{} hit a transient error; retrying in {}s",
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
fn real_sleep(d: std::time::Duration) {
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
fn run_status_with_retry(
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
            "notarize: {} attempt {}/{} failed; retrying in {}s",
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
/// invalid, timeout, and accepted statuses (GoReleaser parity: macos.go
/// differentiates AcceptedStatus, InvalidStatus, RejectedStatus, TimeoutStatus).
fn check_notarize_output(
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
            log.status(&format!("notarize: {} succeeded (accepted)", label));
        } else if combined_lower.contains("timeout") {
            // GoReleaser treats timeout as non-fatal (logs info, no error)
            log.warn(&format!(
                "notarize: {} timed out (submission may still be processing)",
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
        // GoReleaser treats timeout as non-fatal (info log, not error)
        log.warn(&format!(
            "notarize: {} timed out waiting for Apple response (submission may still be processing)",
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
// ---------------------------------------------------------------------------

pub struct NotarizeStage;

impl Stage for NotarizeStage {
    fn name(&self) -> &str {
        "notarize"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("notarize");
        let dry_run = ctx.options.dry_run;

        let notarize_config = match ctx.config.notarize {
            Some(ref cfg) => cfg,
            None => return Ok(()),
        };

        // Respect top-level skip flag. Use try_evaluates_to_true so a malformed
        // skip: template surfaces as Err instead of silently evaluating
        // false and running notarization the user thought was suppressed.
        if let Some(ref d) = notarize_config.skip
            && d.try_evaluates_to_true(|s| ctx.render_template(s))
                .with_context(|| "notarize: evaluate top-level skip expression")?
        {
            log.status("notarization skipped");
            return Ok(());
        }

        // `macos` and `macos_native` are mutually exclusive — they sign and
        // notarize the same artifacts via different toolchains. Refuse a
        // config that populates both so a binary doesn't get signed twice
        // (the second pass would invalidate the first signature).
        let has_cross = notarize_config
            .macos
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        let has_native = notarize_config
            .macos_native
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        if has_cross && has_native {
            bail!(
                "notarize: 'macos' and 'macos_native' cannot both be populated — \
                 they sign and notarize the same artifacts via different toolchains. \
                 Pick one (rcodesign for macos, codesign+notarytool for macos_native)."
            );
        }

        // Cross-platform signing/notarization (rcodesign)
        if let Some(ref macos_configs) = notarize_config.macos {
            for (idx, cfg) in macos_configs.iter().enumerate() {
                run_cross_platform(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Native signing/notarization (codesign + xcrun notarytool)
        if let Some(ref native_configs) = notarize_config.macos_native {
            for (idx, cfg) in native_configs.iter().enumerate() {
                run_native(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Refresh artifact checksums after signing (GoReleaser parity: macos.go:144).
        // Signing modifies binaries in-place, so SHA256 metadata becomes stale.
        if !dry_run {
            refresh_artifact_checksums(ctx, &log);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cross-platform (rcodesign)
// ---------------------------------------------------------------------------

fn run_cross_platform(
    ctx: &Context,
    cfg: &MacOSSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if !is_active(&cfg.skip, ctx) {
        log.status(&format!("notarize: macos[{idx}] skipped (skip: true)"));
        return Ok(());
    }

    // Validate and render sign config
    let sign = cfg
        .sign
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] requires a 'sign' configuration"))?;

    let certificate_raw = ctx
        .render_template_opt(sign.certificate.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.certificate"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.certificate is required"))?;

    // GR docs allow `certificate:` to be either a path OR a base64-encoded
    // P12 blob (the latter is the common shape for storing the cert in a
    // CI secret store). Materialize the base64 form to a tempfile so
    // rcodesign can read it via its `--p12-file` flag.
    let _cert_secret = materialize_secret(&certificate_raw, "sign.certificate")
        .with_context(|| format!("notarize: macos[{idx}] materialize sign.certificate"))?;
    let certificate = _cert_secret.path.clone();

    // Stat-check the resolved path before launching rcodesign so a typo or
    // missing file produces a clean "certificate not found" error instead
    // of an opaque rcodesign exit code partway through artifact
    // iteration. Skipped in dry-run because dry-run never actually
    // invokes rcodesign and upstream callers (incl. tests) commonly
    // point to placeholder paths.
    if !dry_run && !std::path::Path::new(&certificate).exists() {
        bail!("notarize: macos[{idx}] sign.certificate path does not exist: '{certificate}'");
    }

    let password = ctx
        .render_template_opt(sign.password.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.password"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.password is required"))?;

    let entitlements = ctx
        .render_template_opt(sign.entitlements.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.entitlements"))?;

    // Render and validate notarize config fields (if present). The
    // `_key_secret` binding is lifted to the function scope so a
    // materialized base64-decoded tempfile survives until the subprocess
    // launches below.
    let mut _key_secret: Option<MaterializedSecret> = None;
    let notarize_api = if let Some(ref ncfg) = cfg.notarize {
        let issuer_id = ctx.render_template_opt(ncfg.issuer_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.issuer_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.issuer_id is required when notarize block is present")
            })?;
        let key_raw = ctx
            .render_template_opt(ncfg.key.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "notarize: macos[{idx}] notarize.key is required when notarize block is present"
                )
            })?;
        // Same path-or-base64 contract as the certificate above. The
        // .p8 API key is commonly stored as `APPLE_API_KEY=$(cat key.p8 | base64)`
        // in CI; materialize the base64 form to a tempfile that survives
        // until the end of run_cross_platform.
        let secret = materialize_secret(&key_raw, "notarize.key")
            .with_context(|| format!("notarize: macos[{idx}] materialize notarize.key"))?;
        let key = secret.path.clone();
        _key_secret = Some(secret);
        // Stat-check the resolved path before launching rcodesign so a
        // typo or unmounted secret produces a clean "key not found"
        // error instead of an opaque rcodesign exit code partway through
        // artifact iteration. Skipped in dry-run for the same reason as
        // the cert check above.
        if !dry_run && !std::path::Path::new(&key).exists() {
            bail!("notarize: macos[{idx}] notarize.key path does not exist: '{key}'");
        }
        let key_id = ctx.render_template_opt(ncfg.key_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.key_id is required when notarize block is present")
            })?;
        let timeout = Some(ncfg.resolved_timeout());
        Some((issuer_id, key, key_id, ncfg.resolved_wait(), timeout))
    } else {
        None
    };

    // Default IDs to project name when not specified (GoReleaser parity: macos.go:35)
    let ids = cfg.ids.clone().or_else(|| {
        if ctx.config.project_name.is_empty() {
            None
        } else {
            Some(vec![ctx.config.project_name.clone()])
        }
    });

    // Collect darwin Binary + UniversalBinary artifacts, filtered by ids
    let darwin_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary)
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, &ids)
        })
        .collect();

    if darwin_artifacts.is_empty() {
        // Surface the filter contents so misconfigured `ids:` is visible
        // instead of producing a silent no-op.
        log.warn(&format!(
            "notarize: macos[{idx}] ids={:?} matched no darwin binaries \
             (check for typos or unbuilt darwin targets)",
            ids
        ));
        ctx.strict_guard(
            log,
            &format!("notarize: macos[{idx}] no matching darwin binaries found"),
        )?;
        return Ok(());
    }

    for artifact in &darwin_artifacts {
        let binary_path = artifact.path.to_string_lossy();

        // Resolve the timestamp URL once per artifact: per-config override
        // wins over the Apple default.
        let timestamp_url = sign.resolved_timestamp_url();

        let mut sign_args = vec![
            "rcodesign".to_string(),
            "sign".to_string(),
            "--p12-file".to_string(),
            certificate.clone(),
            "--p12-password".to_string(),
            password.clone(),
            "--timestamp-url".to_string(),
            timestamp_url.to_string(),
        ];
        if let Some(ref ent) = entitlements {
            sign_args.push("--entitlements-xml-path".to_string());
            sign_args.push(ent.clone());
        }
        sign_args.push(binary_path.to_string());

        log.status(&format!(
            "notarize: signing {} with rcodesign",
            artifact.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                redact_args(&sign_args, log).join(" ")
            ));
        } else {
            // M6: rcodesign sign contacts Apple's RFC 3161 timestamp server
            // (`http://timestamp.apple.com/ts01`); transient blips there used
            // to fail the whole release. Wrap in the 3-attempt 30s
            // exponential retry.
            let label = format!("rcodesign sign for {}", artifact.name());
            let status = run_status_with_retry(&sign_args, &label, log, &real_sleep)?;
            if !status.success() {
                bail!(
                    "notarize: rcodesign sign failed for {} (exit code: {:?})",
                    artifact.name(),
                    status.code()
                );
            }
        }

        // Notarize if configured
        if let Some((ref issuer_id, ref key, ref key_id, wait, ref timeout)) = notarize_api {
            let mut notarize_args = vec![
                "rcodesign".to_string(),
                "notary-submit".to_string(),
                "--api-issuer".to_string(),
                issuer_id.clone(),
                "--api-key".to_string(),
                key_id.clone(),
                "--api-key-path".to_string(),
                key.clone(),
            ];
            if wait {
                notarize_args.push("--wait".to_string());
                if let Some(t) = timeout {
                    notarize_args.push("--max-wait".to_string());
                    notarize_args.push(t.clone());
                }
            }
            notarize_args.push(binary_path.to_string());

            log.status(&format!(
                "notarize: submitting {} for notarization via rcodesign",
                artifact.name()
            ));

            if dry_run {
                log.status(&format!(
                    "  [dry-run] would run: {}",
                    redact_args(&notarize_args, log).join(" ")
                ));
            } else {
                // M6: wrap in a 3-attempt 30s exponential retry so a
                // transient blip on the App Store Connect API does not fail
                // the whole release (notary-submit talks directly to
                // Apple-hosted services).
                let label = format!("rcodesign notary-submit for {}", artifact.name());
                let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
                check_notarize_output(&output, &label, log)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native (codesign + xcrun notarytool)
// ---------------------------------------------------------------------------

/// Parameters for native signing/notarization, extracted from config before
/// calling the mode-specific functions. Avoids passing many positional args
/// (clippy::too_many_arguments).
struct NativeSignParams<'a> {
    idx: usize,
    identity: &'a str,
    keychain: Option<&'a str>,
    options: Option<&'a [String]>,
    entitlements: Option<&'a str>,
    profile_name: &'a str,
    wait: bool,
    timeout: Option<&'a str>,
    ids: &'a Option<Vec<String>>,
}

fn run_native(
    ctx: &Context,
    cfg: &MacOSNativeSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if !is_active(&cfg.skip, ctx) {
        log.status(&format!(
            "notarize: macos_native[{idx}] skipped (skip: true)"
        ));
        return Ok(());
    }

    use anodizer_core::config::MacOSNativeArtifactKind;
    let artifact_type = cfg.resolved_use();

    // Validate sign config
    let sign = cfg.sign.as_ref().ok_or_else(|| {
        anyhow::anyhow!("notarize: macos_native[{idx}] requires a 'sign' configuration")
    })?;

    let identity = ctx
        .render_template_opt(sign.identity.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.identity"))?
        .ok_or_else(|| {
            anyhow::anyhow!("notarize: macos_native[{idx}] sign.identity is required")
        })?;

    let keychain = ctx
        .render_template_opt(sign.keychain.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.keychain"))?;

    let entitlements = ctx
        .render_template_opt(sign.entitlements.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.entitlements"))?;

    // Validate notarize config
    let notarize = cfg.notarize.as_ref().ok_or_else(|| {
        anyhow::anyhow!("notarize: macos_native[{idx}] requires a 'notarize' configuration")
    })?;

    let profile_name = ctx
        .render_template_opt(notarize.profile_name.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render notarize.profile_name"))?
        .ok_or_else(|| {
            anyhow::anyhow!("notarize: macos_native[{idx}] notarize.profile_name is required")
        })?;

    let wait = notarize.resolved_wait();

    let timeout = Some(notarize.resolved_timeout());

    // Default IDs to project name when not specified (GoReleaser parity: macos.go:35)
    let ids = cfg.ids.clone().or_else(|| {
        if ctx.config.project_name.is_empty() {
            None
        } else {
            Some(vec![ctx.config.project_name.clone()])
        }
    });

    // Issue 9: Warn if options set with use: pkg (options only apply to DMGs)
    if artifact_type == MacOSNativeArtifactKind::Pkg
        && sign.options.as_ref().is_some_and(|o| !o.is_empty())
    {
        log.warn(&format!(
            "notarize: macos_native[{idx}] sign.options is set but only applies to DMG mode; ignored for pkg"
        ));
    }

    let params = NativeSignParams {
        idx,
        identity: &identity,
        keychain: keychain.as_deref(),
        options: sign.options.as_deref(),
        entitlements: entitlements.as_deref(),
        profile_name: &profile_name,
        wait,
        timeout: timeout.as_deref(),
        ids: &ids,
    };

    match artifact_type {
        MacOSNativeArtifactKind::Dmg => run_native_dmg(ctx, &params, dry_run, log),
        MacOSNativeArtifactKind::Pkg => run_native_pkg(ctx, &params, dry_run, log),
    }
}

// ---------------------------------------------------------------------------
// Native DMG mode
// ---------------------------------------------------------------------------

fn run_native_dmg(
    ctx: &Context,
    params: &NativeSignParams,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let idx = params.idx;

    // Find AppBundle (Installer with format=appbundle) artifacts for darwin targets
    let app_bundles: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::Installer
                && a.metadata.get("format").map(|f| f.as_str()) == Some("appbundle")
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    // Sign each app bundle with codesign
    for bundle in &app_bundles {
        let bundle_path = bundle.path.to_string_lossy();

        let mut codesign_args = vec![
            "codesign".to_string(),
            "--deep".to_string(),
            "--force".to_string(),
            "--sign".to_string(),
            params.identity.to_string(),
        ];
        if let Some(kc) = params.keychain {
            codesign_args.push("--keychain".to_string());
            codesign_args.push(kc.to_string());
        }
        if let Some(opts) = params.options
            && !opts.is_empty()
        {
            codesign_args.push("--options".to_string());
            codesign_args.push(opts.join(","));
        }
        if let Some(ent) = params.entitlements {
            codesign_args.push("--entitlements".to_string());
            codesign_args.push(ent.to_string());
        }
        codesign_args.push(bundle_path.to_string());

        log.status(&format!(
            "notarize: signing app bundle {} with codesign",
            bundle.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                codesign_args.join(" ")
            ));
        } else {
            let status = Command::new(&codesign_args[0])
                .args(&codesign_args[1..])
                .status()
                .with_context(|| {
                    format!("notarize: failed to execute codesign for {}", bundle.name())
                })?;
            if !status.success() {
                bail!(
                    "notarize: codesign failed for {} (exit code: {:?})",
                    bundle.name(),
                    status.code()
                );
            }
        }
    }

    // Find DiskImage artifacts for darwin targets and notarize each
    let dmg_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::DiskImage
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    if app_bundles.is_empty() && dmg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "notarize: macos_native[{idx}] (dmg) no matching app bundles or DMGs found \
                 (ids={:?})",
                params.ids
            ),
        )?;
        return Ok(());
    }

    // Warn when app bundles were signed but no DMGs found for notarization
    if !app_bundles.is_empty() && dmg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!("notarize: macos_native[{idx}] signed app bundles but no DMGs found for notarization"),
        )?;
    }

    for dmg in &dmg_artifacts {
        let dmg_path = dmg.path.to_string_lossy();

        // Notarize the DMG
        let mut notarize_args = vec![
            "xcrun".to_string(),
            "notarytool".to_string(),
            "submit".to_string(),
            dmg_path.to_string(),
            "--keychain-profile".to_string(),
            params.profile_name.to_string(),
        ];
        if let Some(kc) = params.keychain {
            notarize_args.push("--keychain".to_string());
            notarize_args.push(kc.to_string());
        }
        if params.wait {
            notarize_args.push("--wait".to_string());
        }
        if let Some(t) = params.timeout {
            notarize_args.push("--timeout".to_string());
            notarize_args.push(t.to_string());
        }

        log.status(&format!(
            "notarize: submitting {} for notarization via xcrun notarytool",
            dmg.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                notarize_args.join(" ")
            ));
        } else {
            // M6: wrap notarytool submit in a 3-attempt 30s exponential
            // retry; the call talks directly to Apple-hosted services and a
            // transient blip should not fail the whole release.
            let label = format!("xcrun notarytool submit for {}", dmg.name());
            let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
            check_notarize_output(&output, &label, log)?;

            // Staple if wait was enabled. Without `wait: true`, the
            // submit returns before Apple completes processing, so the
            // ticket isn't available to staple. Surface that explicitly
            // so a user who expected a stapled DMG knows the publisher
            // skipped that step on purpose.
            if !params.wait {
                log.status(&format!(
                    "notarize: {} submitted (wait disabled; ticket will not be stapled — \
                     end-users will need an internet connection on first launch)",
                    dmg.name()
                ));
            }
            if params.wait {
                let dmg_path_str = dmg_path.to_string();
                let staple_args = ["xcrun", "stapler", "staple", &dmg_path_str];

                log.status(&format!("notarize: stapling {}", dmg.name()));

                let status = Command::new(staple_args[0])
                    .args(&staple_args[1..])
                    .status()
                    .with_context(|| {
                        format!(
                            "notarize: failed to execute xcrun stapler staple for {}",
                            dmg.name()
                        )
                    })?;
                if !status.success() {
                    bail!(
                        "notarize: xcrun stapler staple failed for {} (exit code: {:?})",
                        dmg.name(),
                        status.code()
                    );
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native PKG mode
// ---------------------------------------------------------------------------

fn run_native_pkg(
    ctx: &Context,
    params: &NativeSignParams,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let idx = params.idx;

    // Find MacOsPackage artifacts (excluding appbundles) for darwin targets
    let pkg_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::MacOsPackage
                && a.metadata.get("format").map(|f| f.as_str()) != Some("appbundle")
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    if pkg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "notarize: macos_native[{idx}] (pkg) no matching PKG artifacts found (ids={:?})",
                params.ids
            ),
        )?;
        return Ok(());
    }

    for pkg in &pkg_artifacts {
        let pkg_path = pkg.path.to_string_lossy();

        // Sign with productsign
        let signed_path = format!("{}.signed", pkg_path);
        let mut sign_args = vec![
            "productsign".to_string(),
            "--sign".to_string(),
            params.identity.to_string(),
        ];
        if let Some(kc) = params.keychain {
            sign_args.push("--keychain".to_string());
            sign_args.push(kc.to_string());
        }
        sign_args.push(pkg_path.to_string());
        sign_args.push(signed_path.clone());

        log.status(&format!(
            "notarize: signing {} with productsign",
            pkg.name()
        ));

        if dry_run {
            log.status(&format!("  [dry-run] would run: {}", sign_args.join(" ")));
        } else {
            let status = Command::new(&sign_args[0])
                .args(&sign_args[1..])
                .status()
                .with_context(|| {
                    format!("notarize: failed to execute productsign for {}", pkg.name())
                })?;
            if !status.success() {
                bail!(
                    "notarize: productsign failed for {} (exit code: {:?})",
                    pkg.name(),
                    status.code()
                );
            }

            // Replace the original with the signed version
            std::fs::rename(&signed_path, pkg_path.as_ref()).with_context(|| {
                format!(
                    "notarize: failed to replace {} with signed version",
                    pkg.name()
                )
            })?;
        }

        // Notarize with xcrun notarytool
        let mut notarize_args = vec![
            "xcrun".to_string(),
            "notarytool".to_string(),
            "submit".to_string(),
            pkg_path.to_string(),
            "--keychain-profile".to_string(),
            params.profile_name.to_string(),
        ];
        if let Some(kc) = params.keychain {
            notarize_args.push("--keychain".to_string());
            notarize_args.push(kc.to_string());
        }
        if params.wait {
            notarize_args.push("--wait".to_string());
        }
        if let Some(t) = params.timeout {
            notarize_args.push("--timeout".to_string());
            notarize_args.push(t.to_string());
        }

        log.status(&format!(
            "notarize: submitting {} for notarization via xcrun notarytool",
            pkg.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                notarize_args.join(" ")
            ));
        } else {
            // M6: 3-attempt 30s exponential retry around the Apple-hosted
            // notarytool submit call.
            let label = format!("xcrun notarytool submit for {}", pkg.name());
            let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
            check_notarize_output(&output, &label, log)?;

            // Without `wait: true`, the submit returns before Apple
            // completes processing, so the ticket isn't available to
            // staple. Surface that explicitly so a user who expected a
            // stapled PKG knows the publisher skipped that step on
            // purpose.
            if !params.wait {
                log.status(&format!(
                    "notarize: {} submitted (wait disabled; ticket will not be stapled — \
                     end-users will need an internet connection on first launch)",
                    pkg.name()
                ));
            }
            if params.wait {
                let pkg_path_str = pkg_path.to_string();
                let staple_args = ["xcrun", "stapler", "staple", &pkg_path_str];

                log.status(&format!("notarize: stapling {}", pkg.name()));

                let status = Command::new(staple_args[0])
                    .args(&staple_args[1..])
                    .status()
                    .with_context(|| {
                        format!(
                            "notarize: failed to execute xcrun stapler staple for {}",
                            pkg.name()
                        )
                    })?;
                if !status.success() {
                    bail!(
                        "notarize: xcrun stapler staple failed for {} (exit code: {:?})",
                        pkg.name(),
                        status.code()
                    );
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, MacOSNativeNotarizeConfig, MacOSNativeSignConfig, MacOSNativeSignNotarizeConfig,
        MacOSNotarizeApiConfig, MacOSSignConfig, MacOSSignNotarizeConfig, NotarizeConfig,
        StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};

    // -----------------------------------------------------------------------
    // Config deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_config_deserializes() {
        // Per-config gating uses the canonical `skip:` field; the block
        // below opts in implicitly (no `skip:` = run).
        let yaml = r#"
notarize:
  macos:
    - ids: [myapp]
      sign:
        certificate: /path/to/cert.p12
        password: "s3cret"
        entitlements: entitlements.xml
      notarize:
        issuer_id: "abc-123"
        key: /path/to/key.p8
        key_id: "KEY123"
        timeout: "15m"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let macos = notarize.macos.unwrap();
        assert_eq!(macos.len(), 1);

        let entry = &macos[0];
        assert_eq!(entry.skip, None);
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(sign.certificate, Some("/path/to/cert.p12".to_string()));
        assert_eq!(sign.password, Some("s3cret".to_string()));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_api = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_api.issuer_id, Some("abc-123".to_string()));
        assert_eq!(notarize_api.key, Some("/path/to/key.p8".to_string()));
        assert_eq!(notarize_api.key_id, Some("KEY123".to_string()));
        assert_eq!(
            notarize_api.timeout.map(|d| d.as_humantime_string()),
            Some("15m".to_string())
        );
        assert_eq!(notarize_api.wait, Some(true));
    }

    #[test]
    fn test_native_config_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - use: dmg
      ids: [myapp]
      sign:
        identity: "Developer ID Application: Example"
        keychain: /path/to/keychain
        options: [runtime]
        entitlements: entitlements.xml
      notarize:
        profile_name: "my-profile"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(native.len(), 1);

        let entry = &native[0];
        assert_eq!(entry.skip, None);
        assert_eq!(
            entry.use_,
            Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg)
        );
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(
            sign.identity,
            Some("Developer ID Application: Example".to_string())
        );
        assert_eq!(sign.keychain, Some("/path/to/keychain".to_string()));
        assert_eq!(sign.options, Some(vec!["runtime".to_string()]));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_cfg = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_cfg.profile_name, Some("my-profile".to_string()));
        assert_eq!(notarize_cfg.wait, Some(true));
    }

    #[test]
    fn test_native_config_pkg_mode_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - use: pkg
      sign:
        identity: "Developer ID Installer: Example"
      notarize:
        profile_name: "my-profile"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(
            native[0].use_,
            Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg)
        );
    }

    #[test]
    fn test_skip_string_template_deserializes() {
        // The template form of `skip:` still parses on per-config
        // notarize blocks.
        let yaml = r#"
notarize:
  macos:
    - skip: "{{ if .IsSnapshot }}true{{ endif }}"
      sign:
        certificate: cert.p12
        password: pass
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let macos = config.notarize.unwrap().macos.unwrap();
        match &macos[0].skip {
            Some(StringOrBool::String(s)) => {
                assert_eq!(s, "{{ if .IsSnapshot }}true{{ endif }}")
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_both_modes_in_single_config() {
        let yaml = r#"
notarize:
  macos:
    - sign:
        certificate: cert.p12
        password: pass
  macos_native:
    - sign:
        identity: "Developer ID Application: Test"
      notarize:
        profile_name: test-profile
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_some());
        assert!(notarize.macos_native.is_some());
    }

    #[test]
    fn test_empty_notarize_config_deserializes() {
        let yaml = r#"
notarize: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_none());
        assert!(notarize.macos_native.is_none());
    }

    // -----------------------------------------------------------------------
    // Stage skipping / enabled logic tests
    // -----------------------------------------------------------------------

    fn make_ctx_with_notarize(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_skips_when_no_notarize_config() {
        let config = Config::default();
        let mut ctx = make_ctx_with_notarize(config);

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed with no-op
    }

    #[test]
    fn test_stage_skips_disabled_cross_platform() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed without errors (disabled)
    }

    #[test]
    fn test_stage_skips_disabled_native() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_stage_skips_when_enabled_is_none() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        // Should skip because enabled defaults to false
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Required field validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: None,
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
            "error should mention missing sign config"
        );
    }

    #[test]
    fn test_cross_platform_requires_certificate() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: None,
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.certificate is required"),
        );
    }

    #[test]
    fn test_cross_platform_requires_password() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.password is required"),
        );
    }

    #[test]
    fn test_native_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: None,
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
        );
    }

    #[test]
    fn test_native_requires_identity() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: None,
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.identity is required"),
        );
    }

    #[test]
    fn test_native_requires_notarize_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: None,
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'notarize'"),
        );
    }

    #[test]
    fn test_native_requires_profile_name() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("notarize.profile_name is required"),
        );
    }

    #[test]
    fn test_native_rejects_unsupported_use_type_at_parse_time() {
        // `notarize.macos_native.use` is a typed enum; unsupported values
        // must fail at parse time instead of producing a silent no-op.
        let yaml = r#"
notarize:
  macos_native:
    - use: zip
      sign:
        identity: "Developer ID"
      notarize:
        profile_name: "profile"
crates: []
"#;
        let result: std::result::Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "macos_native.use: zip must be rejected (only 'dmg' / 'pkg' allowed)"
        );
    }

    // -----------------------------------------------------------------------
    // Dry-run behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_dry_run_with_darwin_binaries() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    entitlements: Some("ent.xml".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNotarizeApiConfig {
                    issuer_id: Some("issuer-123".to_string()),
                    key: Some("key.p8".to_string()),
                    key_id: Some("KEY1".to_string()),
                    wait: Some(true),
                    timeout: Some(anodizer_core::config::HumanDuration(
                        std::time::Duration::from_secs(20 * 60),
                    )),
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Also register a linux binary that should be ignored
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_linux"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed without actually invoking rcodesign
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_dry_run_sign_only_no_notarize() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                notarize: None, // sign-only
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_no_darwin_binaries_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only register Linux binaries
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    keychain: Some("/path/to/kc".to_string()),
                    options: Some(vec!["runtime".to_string()]),
                    entitlements: Some("ent.xml".to_string()),
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register an app bundle artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        // Register a DMG artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a MacOsPackage artifact (not appbundle)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::MacOsPackage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.pkg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "pkg".to_string()),
                ("identifier".to_string(), "com.example.myapp".to_string()),
            ]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // No artifacts registered at all
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Artifact filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_ids_filter() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                ids: Some(vec!["other-crate".to_string()]),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This binary is for "myapp" but ids filter is ["other-crate"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed with no-op since id doesn't match
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_matches_ids_helper_no_filter() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(matches_ids(&artifact, &None));
        assert!(matches_ids(&artifact, &Some(vec![])));
    }

    #[test]
    fn test_matches_ids_helper_no_id_metadata_does_not_match() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["other".to_string()])));
    }

    #[test]
    fn test_matches_ids_helper_by_metadata_id() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-arm".to_string())]),
            size: None,
        };

        assert!(matches_ids(&artifact, &Some(vec!["build-arm".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
    }

    #[test]
    fn test_cross_platform_filters_non_darwin_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only non-darwin targets
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // No darwin artifacts, so this is a no-op
    }

    #[test]
    fn test_native_dmg_filters_appbundle_by_ids() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                ids: Some(vec!["other".to_string()]),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This artifact has crate_name "myapp" but ids filter is ["other"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed as no-op since ids don't match
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // is_active helper tests (historical: was is_enabled, inverted)
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_active_none_runs() {
        // None -> run (default opt-in once notarize block is present).
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(is_active(&None, &ctx));
    }

    #[test]
    fn test_is_active_skip_true_skipped() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(!is_active(&Some(StringOrBool::Bool(true)), &ctx));
    }

    #[test]
    fn test_is_active_skip_false_runs() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(is_active(&Some(StringOrBool::Bool(false)), &ctx));
    }

    #[test]
    fn test_is_active_skip_string_true_skipped() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(!is_active(
            &Some(StringOrBool::String("true".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_active_skip_string_false_runs() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(is_active(
            &Some(StringOrBool::String("false".to_string())),
            &ctx
        ));
    }

    // -----------------------------------------------------------------------
    // Native DMG mode defaults to "dmg" when use_ is None
    // -----------------------------------------------------------------------

    #[test]
    fn test_native_defaults_to_dmg_when_use_is_none() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: None, // should default to "dmg"
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a DMG so the stage has something to find (or not)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed because it defaults to DMG mode
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // M6: notarize retry tests
    // -----------------------------------------------------------------------

    /// Build a synthetic `Output` with a non-zero exit and the given stderr,
    /// useful for exercising `is_retriable_notarize_output` without actually
    /// running a process. The exit status is constructed via the os-specific
    /// `from_raw` helpers so we don't need to depend on a child process.
    #[cfg(unix)]
    fn fake_output(stderr: &str, code: i32) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[cfg(unix)]
    fn test_logger() -> anodizer_core::log::StageLogger {
        anodizer_core::log::StageLogger::new("notarize", anodizer_core::log::Verbosity::Quiet)
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_network_markers() {
        // Network-side blips: must classify as retriable.
        let log = test_logger();
        for marker in [
            "tls: bad record",
            "i/o timeout",
            "could not resolve host",
            "503 service unavailable",
            "429 too many requests",
            "dial tcp: connection refused",
            "connection reset by peer",
        ] {
            let out = fake_output(marker, 1);
            assert!(
                is_retriable_notarize_output(&out, &log),
                "should retry on '{marker}'"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_apple_rejection_is_terminal() {
        // Apple-side hard rejections: must NOT retry. Re-submitting an
        // invalid bundle is wasted API quota and worse UX (multi-minute
        // delays before the user sees the real error).
        let log = test_logger();
        for marker in [
            "status: Invalid",
            "Invalid submission",
            "status: Rejected",
            "submission rejected by Apple",
        ] {
            let out = fake_output(marker, 1);
            assert!(
                !is_retriable_notarize_output(&out, &log),
                "must NOT retry on '{marker}'"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_unknown_failure_is_terminal() {
        // An exit failure with no recognised network marker (e.g. malformed
        // CLI args, certificate not found) is treated as terminal — retrying
        // will not help.
        let out = fake_output("error: --p12-file: no such file", 64);
        assert!(!is_retriable_notarize_output(&out, &test_logger()));
    }

    #[cfg(unix)]
    #[test]
    fn test_run_with_retry_returns_immediately_on_terminal_error() {
        // Drive `run_with_retry` through `false`, which exits 1 with no
        // stderr — classifies as non-retriable and should return on the
        // first attempt without invoking the delay function. A no-op delay
        // closure ensures the test cannot accidentally sleep 30s if the
        // classification logic ever drifts.
        let log = anodizer_core::log::StageLogger::new(
            "notarize-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let no_delay = |_d: std::time::Duration| {};
        let args = vec!["false".to_string()];
        let result = run_with_retry(&args, "false-cmd", &log, &no_delay).unwrap();
        assert!(!result.status.success());
    }

    /// `refresh_artifact_checksums` must cover signed DMG and PKG artifacts
    /// in addition to binaries — productsign and stapler rewrite bytes
    /// in place, so any cached `sha256` metadata is stale unless we
    /// recompute it after the signing pipeline.
    #[test]
    fn refresh_artifact_checksums_covers_dmg_and_pkg() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::tempdir().unwrap();

        let dmg_path = tmp.path().join("app.dmg");
        std::fs::write(&dmg_path, b"signed-dmg-bytes").unwrap();
        let pkg_path = tmp.path().join("app.pkg");
        std::fs::write(&pkg_path, b"signed-pkg-bytes").unwrap();

        let mut dmg_md = HashMap::new();
        dmg_md.insert("sha256".to_string(), "stale".to_string());
        let mut pkg_md = HashMap::new();
        pkg_md.insert("sha256".to_string(), "stale".to_string());

        let mut config = Config::default();
        config.project_name = "p".to_string();
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            name: "app.dmg".to_string(),
            path: PathBuf::from(&dmg_path),
            kind: ArtifactKind::DiskImage,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "p".to_string(),
            metadata: dmg_md,
            size: None,
        });
        ctx.artifacts.add(Artifact {
            name: "app.pkg".to_string(),
            path: PathBuf::from(&pkg_path),
            kind: ArtifactKind::MacOsPackage,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "p".to_string(),
            metadata: pkg_md,
            size: None,
        });

        let log = test_logger();
        refresh_artifact_checksums(&mut ctx, &log);

        for art in ctx.artifacts.all() {
            let sha = art.metadata.get("sha256").expect("sha256 set");
            assert_ne!(sha, "stale", "{} sha256 must be refreshed", art.name);
            assert_eq!(sha.len(), 64, "sha256 must be 64 hex chars");
        }
    }
}
