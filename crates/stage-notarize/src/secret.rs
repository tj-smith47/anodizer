//! Pre-sign helpers: artifact-checksum refresh, per-config skip / id
//! gating, and base64 secret materialization + arg redaction.

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::StringOrBool;
use anodizer_core::context::Context;

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
pub(super) fn refresh_artifact_checksums(ctx: &mut Context, log: &anodizer_core::log::StageLogger) {
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
pub(super) fn is_active(skip: &Option<StringOrBool>, ctx: &Context) -> bool {
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
pub(super) fn matches_ids(artifact: &Artifact, ids: &Option<Vec<String>>) -> bool {
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
pub(super) fn looks_like_base64(value: &str) -> bool {
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
pub(super) struct MaterializedSecret {
    /// Path string the caller passes to the subprocess.
    pub(super) path: String,
    /// `Some` when we wrote a tempfile; `None` when we passed the user
    /// path through verbatim. Dropped at the end of the caller's scope
    /// to remove the on-disk decode.
    _tempfile: Option<tempfile::NamedTempFile>,
}

pub(super) fn materialize_secret(value: &str, label: &str) -> Result<MaterializedSecret> {
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
pub(super) fn redact_args(args: &[String], log: &anodizer_core::log::StageLogger) -> Vec<String> {
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
