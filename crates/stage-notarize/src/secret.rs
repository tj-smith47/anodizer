//! Pre-sign helpers: artifact-checksum refresh, per-config skip / id
//! gating, and base64 secret materialization + arg redaction.

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
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

/// Detect whether `value` is an inline base64-encoded P12 / P8 blob (as
/// opposed to a filesystem path to one). The
/// `notarize.macos[*].sign.certificate` and `notarize.macos[*].notarize.key`
/// fields both accept either spelling so the secret can flow through an
/// env-var without a sidecar file.
///
/// The discriminator (path vs. base64), in order:
///
/// 1. Value is non-empty.
/// 2. Value does NOT name an existing file on disk — an extant path is always
///    a path, never an inline blob.
/// 3. Value is longer than a typical bare filename (>= 64 chars) so a short
///    non-existent path like `cert.p12` does not false-positive.
/// 4. Value matches the STANDARD base64 alphabet end-to-end (`[A-Za-z0-9+/=]`,
///    plus newline padding). Crucially `/` is part of that alphabet — a real
///    base64 blob routinely contains it — so it is NOT treated as a path
///    separator. A backslash (`\`), absent from the alphabet, still
///    disqualifies the value (it is a Windows path separator).
/// 5. Value decodes cleanly as standard base64. This is the load-bearing
///    discriminator: a filesystem path that happens to clear the alphabet
///    guard almost never decodes to a whole number of base64 quanta, whereas
///    a real encoded blob always does.
pub(super) fn looks_like_base64(value: &str) -> bool {
    // Trim once: surrounding whitespace (a trailing newline from an env-var
    // export, say) must not skew the length / alphabet checks relative to the
    // decode, which also trims.
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    // An existing file is unambiguously a path. Checked before any base64
    // heuristic so a real on-disk P12/P8 is never decoded as inline bytes.
    if std::path::Path::new(v).is_file() {
        return false;
    }
    // A backslash is a Windows path separator and is not in the base64
    // alphabet; `/` IS in the standard alphabet and must be allowed.
    if v.contains('\\') {
        return false;
    }
    if v.len() < 64 {
        return false;
    }
    let alphabet_ok = v.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' || b == b'\n' || b == b'\r'
    });
    if !alphabet_ok {
        return false;
    }
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(v.replace(['\r', '\n'], "").as_bytes())
        .is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// A real standard-base64 blob containing `/` must be classified as inline
    /// base64, NOT as a filesystem path. Standard base64 includes `/` in its
    /// alphabet, so the old "contains `/` => path" rule misclassified genuine
    /// encoded P12/P8 secrets and produced a bogus "path does not exist".
    #[test]
    fn base64_blob_containing_slash_is_inline_not_path() {
        // Force `/` into the encoded output: byte 0xFF maps to a quantum
        // ending in `/` under the standard alphabet.
        let raw = vec![0xFFu8; 96];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert!(
            encoded.contains('/'),
            "fixture must contain a `/` to exercise the bug; got: {encoded}"
        );
        assert!(
            looks_like_base64(&encoded),
            "a standard-base64 blob with `/` must be treated as inline base64"
        );

        // And it materializes to a tempfile holding the decoded bytes, not the
        // literal string used as a path.
        let secret = materialize_secret(&encoded, "sign.certificate").unwrap();
        assert_ne!(secret.path, encoded);
        let written = std::fs::read(&secret.path).unwrap();
        assert_eq!(written, raw);
    }

    /// An actual on-disk file path resolves as a path (passed through verbatim),
    /// even when it is long enough to clear the length guard.
    #[test]
    fn existing_file_path_is_treated_as_path() {
        let tf = tempfile::NamedTempFile::new().unwrap();
        let path = tf.path().to_str().unwrap().to_string();
        assert!(
            !looks_like_base64(&path),
            "an existing file path must be classified as a path, not base64"
        );
        let secret = materialize_secret(&path, "notarize.key").unwrap();
        assert_eq!(
            secret.path, path,
            "an existing path passes through verbatim"
        );
    }

    /// A short bare filename (e.g. `cert.p12`) is a path, not base64 — below
    /// the length guard.
    #[test]
    fn short_filename_is_path() {
        assert!(!looks_like_base64("cert.p12"));
    }

    /// A Windows-style path with backslashes is a path, not base64.
    #[test]
    fn windows_path_is_path() {
        let win = "C:\\Users\\ci\\secrets\\developer-id-application-certificate.p12";
        assert!(!looks_like_base64(win));
    }

    /// An empty value is never base64.
    #[test]
    fn empty_is_not_base64() {
        assert!(!looks_like_base64(""));
    }
}
