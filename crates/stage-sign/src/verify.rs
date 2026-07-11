//! Post-sign signature verification.
//!
//! Signing exiting 0 only proves the signer ran; it does not prove the
//! produced signature actually validates against the artifact. After each
//! signature is written, the stage re-verifies it with the matching
//! verifier so a corrupted signature, a wrong key, or a mismatched keyless
//! certificate fails the release at the sign stage instead of shipping:
//!
//! - keyed cosign → `cosign verify-blob --key <pubkey> …` against a public
//!   key derived once per config via `cosign public-key --key <ref>`,
//! - keyless cosign → `cosign verify-blob --certificate-identity …
//!   --certificate-oidc-issuer …`, with identity/issuer derived from the
//!   ambient GitHub Actions OIDC environment when not configured,
//! - gpg → `gpg --verify <sig> <artifact>` against the same keyring that
//!   signed.
//!
//! Verification inputs that cannot be derived produce an *honest skip* (a
//! verbose line naming exactly what was missing), never a silent pass and
//! never a spurious failure.

use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodizer_core::EnvSource;
use anodizer_core::config::SignVerifyConfig;
use anodizer_core::log::StageLogger;

/// OIDC issuer of GitHub Actions workflow identity tokens — the issuer
/// Fulcio records in every certificate minted from an Actions job.
pub(crate) const GITHUB_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Fallback server URL when `GITHUB_ACTIONS` is set but `GITHUB_SERVER_URL`
/// is not (github.com is the only host that omits it in practice).
const DEFAULT_GITHUB_SERVER_URL: &str = "https://github.com";

/// How the keyless certificate identity is asserted on the verify argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentitySelector {
    /// `--certificate-identity <value>` — exact SAN match.
    Exact(String),
    /// `--certificate-identity-regexp <value>` — pattern match.
    Regexp(String),
}

/// Per-config verification mode, resolved once before the per-artifact
/// fan-out so skip reasons are logged a single time and the keyed public
/// key is derived a single time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigVerifyMode {
    /// `verify: { enabled: false }` — operator opt-out.
    Disabled,
    /// Verification inputs are not derivable; carries the reason for the
    /// verbose skip line.
    Skip(String),
    /// `gpg --verify <sig> <artifact>` against the signing keyring.
    Gpg,
    /// `cosign verify-blob --key <derived pubkey> …`.
    CosignKeyed {
        /// The `--key` reference from the sign argv (e.g.
        /// `env://COSIGN_KEY`), from which the public key is derived.
        key_ref: String,
        /// The sign argv wrote a sigstore bundle (`--bundle`), so verify
        /// consumes `--bundle <sig>` instead of `--signature <sig>`.
        bundle: bool,
        /// The sign argv disabled the transparency-log upload
        /// (`--tlog-upload=false`), so no tlog entry exists to check —
        /// verify must pass `--insecure-ignore-tlog=true` or it would
        /// fail on a signature that is perfectly valid.
        ignore_tlog: bool,
    },
    /// `cosign verify-blob --certificate-identity … --certificate-oidc-issuer …`.
    CosignKeyless {
        identity: IdentitySelector,
        issuer: String,
        /// See [`ConfigVerifyMode::CosignKeyed::bundle`].
        bundle: bool,
        /// The sign config renders a `--output-certificate` path, needed
        /// for non-bundle keyless verification.
        has_certificate: bool,
        /// See [`ConfigVerifyMode::CosignKeyed::ignore_tlog`].
        ignore_tlog: bool,
    },
}

/// A fully-materialized verification command for one signed artifact,
/// executed right after its sign job succeeds.
pub(crate) struct VerifyJob {
    /// Verifier binary — the same resolved `cmd` the sign job used, so a
    /// test double or a pinned cosign path verifies with itself.
    pub(crate) cmd: String,
    pub(crate) args: Vec<String>,
    /// Same rendered env the sign job ran under (key material, redaction
    /// set), plus the cosign consent var.
    pub(crate) env: Option<Vec<(String, String)>>,
    /// Artifact display string for log lines.
    pub(crate) what: String,
}

/// Return the value of `--<flag> <v>` / `--<flag>=<v>` in `args`, if present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&eq_prefix) {
            return Some(v.to_string());
        }
    }
    None
}

/// True when `args` carries `--<flag>` in either spelling.
fn has_flag(args: &[String], flag: &str) -> bool {
    let eq_prefix = format!("{flag}=");
    args.iter().any(|a| a == flag || a.starts_with(&eq_prefix))
}

/// True when the sign argv disabled the transparency-log upload, meaning no
/// tlog entry exists for the signature and verification must not demand one.
fn tlog_upload_disabled(args: &[String]) -> bool {
    flag_value(args, "--tlog-upload").is_some_and(|v| v == "false")
}

/// True when `cmd`'s basename is a gpg binary (`gpg`, `gpg2`, absolute
/// paths thereto). Delegates to the canonical core predicate.
fn is_gpg_cmd(cmd: &str) -> bool {
    anodizer_core::signing::is_gpg_command(cmd)
}

/// Derive the keyless certificate identity + issuer.
///
/// Precedence: explicit config always wins; otherwise the ambient GitHub
/// Actions OIDC environment is consulted — Fulcio records the workflow
/// identity `<server>/<workflow_ref>` (e.g.
/// `https://github.com/acme/app/.github/workflows/release.yml@refs/tags/v1`)
/// as the certificate SAN, and `GITHUB_WORKFLOW_REF` carries exactly the
/// `<owner>/<repo>/<path>@<ref>` tail of it. When the identity cannot be
/// derived but the issuer was resolved *ambiently* from GitHub Actions, fall
/// back to matching any identity under that issuer — strictly weaker, but
/// still proves the certificate chains to Fulcio via GitHub's OIDC. A
/// *config-pinned* issuer with no identity is refused instead of widened to
/// `.*`, since silently accepting any certificate under a pinned issuer
/// inverts the operator's intent. When even the issuer is unknown, return the
/// skip reason naming what was missing.
pub(crate) fn resolve_keyless_identity(
    cfg: Option<&SignVerifyConfig>,
    env: &dyn EnvSource,
) -> Result<(IdentitySelector, String), String> {
    let on_github_actions = env.var("GITHUB_ACTIONS").is_some_and(|v| v == "true");

    let issuer_from_config = cfg.and_then(|c| c.certificate_oidc_issuer.clone());
    let issuer = issuer_from_config
        .clone()
        .or_else(|| on_github_actions.then(|| GITHUB_OIDC_ISSUER.to_string()));

    let identity = cfg
        .and_then(|c| c.certificate_identity.clone())
        .map(IdentitySelector::Exact)
        .or_else(|| {
            cfg.and_then(|c| c.certificate_identity_regexp.clone())
                .map(IdentitySelector::Regexp)
        })
        .or_else(|| {
            if !on_github_actions {
                return None;
            }
            env.var("GITHUB_WORKFLOW_REF").map(|workflow_ref| {
                let server = env
                    .var("GITHUB_SERVER_URL")
                    .unwrap_or_else(|| DEFAULT_GITHUB_SERVER_URL.to_string());
                IdentitySelector::Exact(format!("{server}/{workflow_ref}"))
            })
        });

    match (identity, issuer) {
        (Some(id), Some(iss)) => Ok((id, iss)),
        // Only the ambient GitHub Actions issuer is known and the workflow
        // identity could not be derived (a degraded Actions env with
        // GITHUB_WORKFLOW_REF unset). `.*` under GitHub's OIDC issuer is
        // strictly weaker but still constrains to certificates GitHub's
        // Fulcio path issued — keep the documented fallback for this case.
        (None, Some(iss)) if on_github_actions && issuer_from_config.is_none() => {
            Ok((IdentitySelector::Regexp(".*".to_string()), iss))
        }
        // The issuer was explicitly pinned but no identity was given.
        // Defaulting the identity to `.*` here would silently accept EVERY
        // certificate that issuer ever signed — the opposite of the
        // tightening an operator who pins an issuer intends. Refuse rather
        // than widen; an operator who genuinely wants any-identity-under-issuer
        // opts in explicitly with `certificate_identity_regexp: ".*"`.
        (None, Some(iss)) => Err(format!(
            "keyless verify: `verify.certificate_oidc_issuer` is pinned to `{iss}` but \
             no identity was given. Set `verify.certificate_identity` (exact SAN) or \
             `verify.certificate_identity_regexp` (use `.*` to deliberately accept any \
             identity under that issuer). Refusing to default the identity to `.*`, \
             which would accept every certificate the issuer ever signed."
        )),
        (_, None) => Err(
            "keyless certificate identity/issuer not derivable (not running under \
             GitHub Actions OIDC and `verify.certificate_identity` / \
             `verify.certificate_oidc_issuer` are unset)"
                .to_string(),
        ),
    }
}

/// Resolve a sign config's verification mode from its resolved `cmd`, its
/// (hardened, unrendered) argv, and the ambient environment. Pure — no
/// subprocess, no filesystem — so it is fully unit-testable offline.
pub(crate) fn resolve_config_verify_mode(
    verify_cfg: Option<&SignVerifyConfig>,
    cmd: &str,
    sign_args: &[String],
    has_certificate: bool,
    env: &dyn EnvSource,
) -> ConfigVerifyMode {
    if verify_cfg.is_some_and(|v| !v.is_enabled()) {
        return ConfigVerifyMode::Disabled;
    }
    if is_gpg_cmd(cmd) {
        return ConfigVerifyMode::Gpg;
    }
    if !crate::process::is_cosign_cmd(cmd) {
        return ConfigVerifyMode::Skip(format!(
            "automatic verification supports cosign and gpg; signer '{cmd}' is not recognized"
        ));
    }
    let bundle = has_flag(sign_args, "--bundle");
    let ignore_tlog = tlog_upload_disabled(sign_args);
    if let Some(key_ref) = flag_value(sign_args, "--key") {
        return ConfigVerifyMode::CosignKeyed {
            key_ref,
            bundle,
            ignore_tlog,
        };
    }
    // Keyless: without a sigstore bundle, verification needs the signing
    // certificate the sign step emitted (`certificate:` on the config
    // renders an `--output-certificate` path).
    if !bundle && !has_certificate {
        return ConfigVerifyMode::Skip(
            "keyless signature has neither a `--bundle` output nor a `certificate:` \
             output to verify against"
                .to_string(),
        );
    }
    match resolve_keyless_identity(verify_cfg, env) {
        Ok((identity, issuer)) => ConfigVerifyMode::CosignKeyless {
            identity,
            issuer,
            bundle,
            has_certificate,
            ignore_tlog,
        },
        Err(reason) => ConfigVerifyMode::Skip(reason),
    }
}

/// Append the identity flags for `selector` + `issuer` to a cosign argv.
fn push_identity_flags(args: &mut Vec<String>, selector: &IdentitySelector, issuer: &str) {
    match selector {
        IdentitySelector::Exact(id) => {
            args.push("--certificate-identity".to_string());
            args.push(id.clone());
        }
        IdentitySelector::Regexp(re) => {
            args.push("--certificate-identity-regexp".to_string());
            args.push(re.clone());
        }
    }
    args.push("--certificate-oidc-issuer".to_string());
    args.push(issuer.to_string());
}

/// Build the `cosign verify-blob` / `gpg --verify` argv for one signed
/// artifact. Returns `None` for the non-running modes (`Disabled` / `Skip`,
/// both already logged at config level).
///
/// `pubkey_path` is the derived public key file for
/// [`ConfigVerifyMode::CosignKeyed`] (ignored otherwise); `certificate` is
/// the rendered `--output-certificate` path for non-bundle keyless configs.
pub(crate) fn build_blob_verify_args(
    mode: &ConfigVerifyMode,
    artifact: &str,
    signature: &str,
    certificate: Option<&str>,
    pubkey_path: Option<&str>,
) -> Option<Vec<String>> {
    match mode {
        ConfigVerifyMode::Disabled | ConfigVerifyMode::Skip(_) => None,
        ConfigVerifyMode::Gpg => Some(vec![
            "--verify".to_string(),
            signature.to_string(),
            artifact.to_string(),
        ]),
        ConfigVerifyMode::CosignKeyed {
            bundle,
            ignore_tlog,
            ..
        } => {
            let mut args = vec!["verify-blob".to_string(), "--key".to_string()];
            args.push(pubkey_path.unwrap_or_default().to_string());
            args.push(if *bundle { "--bundle" } else { "--signature" }.to_string());
            args.push(signature.to_string());
            if *ignore_tlog {
                args.push("--insecure-ignore-tlog=true".to_string());
            }
            args.push(artifact.to_string());
            Some(args)
        }
        ConfigVerifyMode::CosignKeyless {
            identity,
            issuer,
            bundle,
            has_certificate,
            ignore_tlog,
        } => {
            let mut args = vec!["verify-blob".to_string()];
            if *bundle {
                args.push("--bundle".to_string());
                args.push(signature.to_string());
            } else {
                if *has_certificate {
                    args.push("--certificate".to_string());
                    args.push(certificate.unwrap_or_default().to_string());
                }
                args.push("--signature".to_string());
                args.push(signature.to_string());
            }
            push_identity_flags(&mut args, identity, issuer);
            if *ignore_tlog {
                args.push("--insecure-ignore-tlog=true".to_string());
            }
            args.push(artifact.to_string());
            Some(args)
        }
    }
}

/// Build the `cosign verify` argv for a registry-attached docker-image
/// signature. The image was pushed and signed moments ago, so registry
/// access is a given at this point. Returns `None` for non-running modes
/// and for gpg (docker signing is cosign-only).
pub(crate) fn build_docker_verify_args(
    mode: &ConfigVerifyMode,
    signed_ref: &str,
    pubkey_path: Option<&str>,
) -> Option<Vec<String>> {
    match mode {
        ConfigVerifyMode::Disabled | ConfigVerifyMode::Skip(_) | ConfigVerifyMode::Gpg => None,
        ConfigVerifyMode::CosignKeyed {
            ignore_tlog: it, ..
        } => {
            let mut args = vec!["verify".to_string(), "--key".to_string()];
            args.push(pubkey_path.unwrap_or_default().to_string());
            if *it {
                args.push("--insecure-ignore-tlog=true".to_string());
            }
            args.push(signed_ref.to_string());
            Some(args)
        }
        ConfigVerifyMode::CosignKeyless {
            identity,
            issuer,
            ignore_tlog: it,
            ..
        } => {
            let mut args = vec!["verify".to_string()];
            push_identity_flags(&mut args, identity, issuer);
            if *it {
                args.push("--insecure-ignore-tlog=true".to_string());
            }
            args.push(signed_ref.to_string());
            Some(args)
        }
    }
}

/// Derive the public half of the cosign key referenced by `key_ref` into
/// `out_path` via `<cmd> public-key --key=<ref> --outfile <path>`.
///
/// A purely local key-load round-trip (same invocation the preflight gate
/// uses in `keyload.rs`) — no network, no tlog. Runs under the sign
/// config's rendered env so `env://VAR` refs and `COSIGN_PASSWORD` resolve
/// exactly as they did for signing; ambient process env still applies for
/// anything not overridden. Called once per sign config, not per artifact.
pub(crate) fn derive_cosign_public_key(
    cmd: &str,
    key_ref: &str,
    env: Option<&[(String, String)]>,
    out_path: &std::path::Path,
) -> Result<()> {
    let mut command = Command::new(cmd);
    command
        .arg("public-key")
        .arg(format!("--key={key_ref}"))
        .arg("--outfile")
        .arg(out_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pairs) = env {
        for (k, v) in pairs {
            command.env(k, v);
        }
    }
    command.env(crate::process::COSIGN_CONSENT_ENV, "true");
    let output = command
        .output()
        .with_context(|| format!("sign verify: failed to spawn '{cmd} public-key'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "sign verify: deriving the public key for '{key_ref}' failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

/// Execute one verification job: spawn, capture, redact, and fail on a
/// non-zero verifier exit. The full argv is verbose-only detail; the
/// default-level signal is the per-config `verified N signature(s)` result
/// the caller emits.
pub(crate) fn execute_verify_job(job: &VerifyJob, log: &StageLogger) -> Result<()> {
    log.verbose(&format!(
        "verifying {}: {} {}",
        job.what,
        job.cmd,
        job.args.join(" ")
    ));
    let mut command = Command::new(&job.cmd);
    command
        .args(&job.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ref env_vars) = job.env {
        for (k, v) in env_vars {
            command.env(k, v);
        }
    }
    let output = command
        .output()
        .with_context(|| format!("verify: failed to run '{}' for {}", job.cmd, job.what))?;

    // Same redaction discipline as the sign path: scrub the job env and the
    // process env from the verifier's stdio before it can reach a log line
    // or an error message.
    let env_pairs: Vec<(String, String)> = job
        .env
        .iter()
        .flat_map(|m| m.iter().cloned())
        .chain(std::env::vars())
        .collect();
    let mut redacted = output;
    redacted.stdout =
        anodizer_core::redact::string(&String::from_utf8_lossy(&redacted.stdout), &env_pairs)
            .into_bytes();
    redacted.stderr =
        anodizer_core::redact::string(&String::from_utf8_lossy(&redacted.stderr), &env_pairs)
            .into_bytes();

    log.check_output(redacted, &job.cmd)
        .with_context(|| format!("signature verification failed for {}", job.what))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::MapEnvSource;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ---- flag parsing ----

    #[test]
    fn flag_value_supports_both_spellings() {
        assert_eq!(
            flag_value(&strs(&["--key", "env://COSIGN_KEY"]), "--key"),
            Some("env://COSIGN_KEY".to_string())
        );
        assert_eq!(
            flag_value(&strs(&["--key=cosign.key"]), "--key"),
            Some("cosign.key".to_string())
        );
        assert_eq!(flag_value(&strs(&["sign-blob", "--yes"]), "--key"), None);
    }

    #[test]
    fn tlog_disabled_detection() {
        assert!(tlog_upload_disabled(&strs(&["--tlog-upload=false"])));
        assert!(tlog_upload_disabled(&strs(&["--tlog-upload", "false"])));
        assert!(!tlog_upload_disabled(&strs(&["--tlog-upload=true"])));
        assert!(!tlog_upload_disabled(&strs(&["sign-blob"])));
    }

    // ---- keyless identity derivation ----

    #[test]
    fn keyless_identity_derived_from_github_actions_env() {
        let env = MapEnvSource::new()
            .with("GITHUB_ACTIONS", "true")
            .with("GITHUB_SERVER_URL", "https://github.com")
            .with(
                "GITHUB_WORKFLOW_REF",
                "acme/app/.github/workflows/release.yml@refs/tags/v1.0.0",
            );
        let (id, issuer) = resolve_keyless_identity(None, &env).expect("derivable");
        assert_eq!(
            id,
            IdentitySelector::Exact(
                "https://github.com/acme/app/.github/workflows/release.yml@refs/tags/v1.0.0"
                    .to_string()
            )
        );
        assert_eq!(issuer, GITHUB_OIDC_ISSUER);
    }

    #[test]
    fn keyless_identity_server_url_defaults_to_github_com() {
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true").with(
            "GITHUB_WORKFLOW_REF",
            "o/r/.github/workflows/w.yml@refs/heads/m",
        );
        let (id, _) = resolve_keyless_identity(None, &env).expect("derivable");
        assert_eq!(
            id,
            IdentitySelector::Exact(
                "https://github.com/o/r/.github/workflows/w.yml@refs/heads/m".to_string()
            )
        );
    }

    #[test]
    fn keyless_identity_config_overrides_env() {
        let cfg = SignVerifyConfig {
            certificate_identity: Some("mailto:release@acme.example".to_string()),
            certificate_oidc_issuer: Some("https://accounts.example.com".to_string()),
            ..Default::default()
        };
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true").with(
            "GITHUB_WORKFLOW_REF",
            "o/r/.github/workflows/w.yml@refs/heads/m",
        );
        let (id, issuer) = resolve_keyless_identity(Some(&cfg), &env).expect("derivable");
        assert_eq!(
            id,
            IdentitySelector::Exact("mailto:release@acme.example".to_string())
        );
        assert_eq!(issuer, "https://accounts.example.com");
    }

    #[test]
    fn keyless_identity_regexp_config_used_when_no_exact() {
        let cfg = SignVerifyConfig {
            certificate_identity_regexp: Some("^https://github.com/acme/.*$".to_string()),
            ..Default::default()
        };
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true");
        let (id, issuer) = resolve_keyless_identity(Some(&cfg), &env).expect("derivable");
        assert_eq!(
            id,
            IdentitySelector::Regexp("^https://github.com/acme/.*$".to_string())
        );
        assert_eq!(issuer, GITHUB_OIDC_ISSUER);
    }

    #[test]
    fn keyless_identity_regexp_fallback_when_issuer_known_but_identity_not() {
        // On Actions but GITHUB_WORKFLOW_REF missing: the issuer is known,
        // so the honest fallback is any-identity-under-that-issuer.
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true");
        let (id, issuer) = resolve_keyless_identity(None, &env).expect("derivable");
        assert_eq!(id, IdentitySelector::Regexp(".*".to_string()));
        assert_eq!(issuer, GITHUB_OIDC_ISSUER);
    }

    #[test]
    fn keyless_identity_skips_when_nothing_derivable() {
        let env = MapEnvSource::new();
        let err = resolve_keyless_identity(None, &env).expect_err("must not be derivable");
        assert!(
            err.contains("certificate_identity"),
            "skip reason must name the missing config: {err}"
        );
    }

    #[test]
    fn keyless_config_issuer_without_identity_refuses_rather_than_widening() {
        // Pinning only the issuer (off Actions) must NOT silently default the
        // identity to `.*` — that would accept every certificate the issuer
        // ever signed, inverting the operator's intent.
        let cfg = SignVerifyConfig {
            certificate_oidc_issuer: Some("https://accounts.example.com".to_string()),
            ..Default::default()
        };
        let env = MapEnvSource::new();
        let err = resolve_keyless_identity(Some(&cfg), &env)
            .expect_err("issuer-only config must not widen to .*");
        assert!(
            err.contains("no identity was given") && err.contains("certificate_identity"),
            "refusal must name the missing identity and the escape hatch: {err}"
        );
    }

    #[test]
    fn keyless_config_issuer_without_identity_refuses_even_on_actions() {
        // A config-pinned issuer is explicit intent regardless of environment;
        // a degraded Actions env (no workflow ref) must not turn it into `.*`.
        let cfg = SignVerifyConfig {
            certificate_oidc_issuer: Some("https://accounts.example.com".to_string()),
            ..Default::default()
        };
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true");
        resolve_keyless_identity(Some(&cfg), &env)
            .expect_err("config-pinned issuer with no identity must refuse, not widen");
    }

    #[test]
    fn keyless_explicit_any_identity_regexp_is_honored() {
        // The any-identity-under-issuer capability is preserved via explicit
        // opt-in: `certificate_identity_regexp: ".*"` flows straight through.
        let cfg = SignVerifyConfig {
            certificate_identity_regexp: Some(".*".to_string()),
            certificate_oidc_issuer: Some("https://accounts.example.com".to_string()),
            ..Default::default()
        };
        let env = MapEnvSource::new();
        let (id, issuer) = resolve_keyless_identity(Some(&cfg), &env).expect("explicit opt-in");
        assert_eq!(id, IdentitySelector::Regexp(".*".to_string()));
        assert_eq!(issuer, "https://accounts.example.com");
    }

    // ---- config mode resolution ----

    #[test]
    fn mode_disabled_when_config_opts_out() {
        let cfg = SignVerifyConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let mode = resolve_config_verify_mode(
            Some(&cfg),
            "cosign",
            &strs(&["sign-blob", "--key=env://K", "x"]),
            false,
            &MapEnvSource::new(),
        );
        assert_eq!(mode, ConfigVerifyMode::Disabled);
    }

    #[test]
    fn mode_gpg_for_gpg_cmds() {
        for cmd in ["gpg", "gpg2", "/usr/local/bin/gpg"] {
            let mode =
                resolve_config_verify_mode(None, cmd, &strs(&[]), false, &MapEnvSource::new());
            assert_eq!(mode, ConfigVerifyMode::Gpg, "cmd {cmd}");
        }
    }

    #[test]
    fn mode_skips_unrecognized_signer() {
        let mode =
            resolve_config_verify_mode(None, "notation", &strs(&[]), false, &MapEnvSource::new());
        assert!(
            matches!(mode, ConfigVerifyMode::Skip(ref r) if r.contains("notation")),
            "got {mode:?}"
        );
    }

    #[test]
    fn mode_keyed_cosign_extracts_key_ref_bundle_and_tlog() {
        let mode = resolve_config_verify_mode(
            None,
            "cosign",
            &strs(&[
                "sign-blob",
                "--key=env://COSIGN_KEY",
                "--bundle={{ Signature }}",
                "--yes",
                "{{ Artifact }}",
                "--tlog-upload=false",
            ]),
            false,
            &MapEnvSource::new(),
        );
        assert_eq!(
            mode,
            ConfigVerifyMode::CosignKeyed {
                key_ref: "env://COSIGN_KEY".to_string(),
                bundle: true,
                ignore_tlog: true,
            }
        );
    }

    #[test]
    fn mode_keyless_requires_bundle_or_certificate() {
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true");
        let mode = resolve_config_verify_mode(
            None,
            "cosign",
            &strs(&["sign-blob", "--output-signature", "{{ Signature }}", "x"]),
            false,
            &env,
        );
        assert!(
            matches!(mode, ConfigVerifyMode::Skip(ref r) if r.contains("certificate")),
            "got {mode:?}"
        );
    }

    #[test]
    fn mode_keyless_with_bundle_on_actions_resolves() {
        let env = MapEnvSource::new().with("GITHUB_ACTIONS", "true").with(
            "GITHUB_WORKFLOW_REF",
            "o/r/.github/workflows/w.yml@refs/tags/v1",
        );
        let mode = resolve_config_verify_mode(
            None,
            "cosign",
            &strs(&[
                "sign-blob",
                "--bundle={{ Signature }}",
                "--yes",
                "{{ Artifact }}",
            ]),
            false,
            &env,
        );
        match mode {
            ConfigVerifyMode::CosignKeyless {
                identity,
                issuer,
                bundle,
                has_certificate,
                ignore_tlog,
            } => {
                assert_eq!(
                    identity,
                    IdentitySelector::Exact(
                        "https://github.com/o/r/.github/workflows/w.yml@refs/tags/v1".to_string()
                    )
                );
                assert_eq!(issuer, GITHUB_OIDC_ISSUER);
                assert!(bundle);
                assert!(!has_certificate);
                assert!(!ignore_tlog);
            }
            other => panic!("expected keyless mode, got {other:?}"),
        }
    }

    #[test]
    fn mode_keyless_skips_off_actions_without_config() {
        let mode = resolve_config_verify_mode(
            None,
            "cosign",
            &strs(&["sign-blob", "--bundle=x.sig", "artifact"]),
            false,
            &MapEnvSource::new(),
        );
        assert!(matches!(mode, ConfigVerifyMode::Skip(_)), "got {mode:?}");
    }

    // ---- argv construction ----

    #[test]
    fn keyed_verify_blob_argv() {
        let mode = ConfigVerifyMode::CosignKeyed {
            key_ref: "env://COSIGN_KEY".to_string(),
            bundle: false,
            ignore_tlog: false,
        };
        let args = build_blob_verify_args(
            &mode,
            "dist/app.tar.gz",
            "dist/app.tar.gz.sig",
            None,
            Some("/tmp/derived.pub"),
        )
        .expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify-blob",
                "--key",
                "/tmp/derived.pub",
                "--signature",
                "dist/app.tar.gz.sig",
                "dist/app.tar.gz",
            ])
        );
    }

    #[test]
    fn keyed_bundle_verify_blob_argv_with_ignored_tlog() {
        let mode = ConfigVerifyMode::CosignKeyed {
            key_ref: "env://COSIGN_KEY".to_string(),
            bundle: true,
            ignore_tlog: true,
        };
        let args = build_blob_verify_args(
            &mode,
            "dist/app",
            "dist/app.sig",
            None,
            Some("/tmp/derived.pub"),
        )
        .expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify-blob",
                "--key",
                "/tmp/derived.pub",
                "--bundle",
                "dist/app.sig",
                "--insecure-ignore-tlog=true",
                "dist/app",
            ])
        );
    }

    #[test]
    fn keyless_bundle_verify_blob_argv() {
        let mode = ConfigVerifyMode::CosignKeyless {
            identity: IdentitySelector::Exact("https://github.com/o/r/w@v".to_string()),
            issuer: GITHUB_OIDC_ISSUER.to_string(),
            bundle: true,
            has_certificate: false,
            ignore_tlog: false,
        };
        let args =
            build_blob_verify_args(&mode, "dist/app", "dist/app.sig", None, None).expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify-blob",
                "--bundle",
                "dist/app.sig",
                "--certificate-identity",
                "https://github.com/o/r/w@v",
                "--certificate-oidc-issuer",
                GITHUB_OIDC_ISSUER,
                "dist/app",
            ])
        );
    }

    #[test]
    fn keyless_certificate_verify_blob_argv_with_regexp_fallback() {
        let mode = ConfigVerifyMode::CosignKeyless {
            identity: IdentitySelector::Regexp(".*".to_string()),
            issuer: GITHUB_OIDC_ISSUER.to_string(),
            bundle: false,
            has_certificate: true,
            ignore_tlog: false,
        };
        let args = build_blob_verify_args(
            &mode,
            "dist/app",
            "dist/app.sig",
            Some("dist/app.pem"),
            None,
        )
        .expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify-blob",
                "--certificate",
                "dist/app.pem",
                "--signature",
                "dist/app.sig",
                "--certificate-identity-regexp",
                ".*",
                "--certificate-oidc-issuer",
                GITHUB_OIDC_ISSUER,
                "dist/app",
            ])
        );
    }

    #[test]
    fn gpg_verify_argv() {
        let args = build_blob_verify_args(
            &ConfigVerifyMode::Gpg,
            "dist/checksums.txt",
            "dist/checksums.txt.sig",
            None,
            None,
        )
        .expect("runs");
        assert_eq!(
            args,
            strs(&["--verify", "dist/checksums.txt.sig", "dist/checksums.txt"])
        );
    }

    #[test]
    fn skip_and_disabled_modes_build_nothing() {
        for mode in [
            ConfigVerifyMode::Disabled,
            ConfigVerifyMode::Skip("reason".to_string()),
        ] {
            assert!(build_blob_verify_args(&mode, "a", "s", None, None).is_none());
            assert!(build_docker_verify_args(&mode, "ref", None).is_none());
        }
    }

    #[test]
    fn docker_keyed_verify_argv() {
        let mode = ConfigVerifyMode::CosignKeyed {
            key_ref: "env://COSIGN_KEY".to_string(),
            bundle: false,
            ignore_tlog: false,
        };
        let args = build_docker_verify_args(
            &mode,
            "ghcr.io/acme/app:v1@sha256:abc",
            Some("/tmp/derived.pub"),
        )
        .expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify",
                "--key",
                "/tmp/derived.pub",
                "ghcr.io/acme/app:v1@sha256:abc",
            ])
        );
    }

    #[test]
    fn docker_keyless_verify_argv() {
        let mode = ConfigVerifyMode::CosignKeyless {
            identity: IdentitySelector::Exact("https://github.com/o/r/w@v".to_string()),
            issuer: GITHUB_OIDC_ISSUER.to_string(),
            bundle: false,
            has_certificate: false,
            ignore_tlog: false,
        };
        let args =
            build_docker_verify_args(&mode, "ghcr.io/acme/app:v1@sha256:abc", None).expect("runs");
        assert_eq!(
            args,
            strs(&[
                "verify",
                "--certificate-identity",
                "https://github.com/o/r/w@v",
                "--certificate-oidc-issuer",
                GITHUB_OIDC_ISSUER,
                "ghcr.io/acme/app:v1@sha256:abc",
            ])
        );
    }

    #[test]
    fn docker_gpg_mode_builds_nothing() {
        assert!(build_docker_verify_args(&ConfigVerifyMode::Gpg, "ref", None).is_none());
    }

    // ---- pubkey derivation invocation shape ----

    /// The derivation must spawn `<cmd> public-key --key=<ref> --outfile
    /// <path>` with the job env applied. Proven with a fake `cosign` script
    /// that records its argv, so no real cosign is needed.
    #[cfg(unix)]
    #[test]
    fn derive_public_key_invocation_shape() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("cosign");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$RECORD_FILE\"\nexit 0\n",
        )
        .expect("write script");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");

        let record = tmp.path().join("argv.txt");
        let out = tmp.path().join("derived.pub");
        let env = vec![(
            "RECORD_FILE".to_string(),
            record.to_string_lossy().to_string(),
        )];
        derive_cosign_public_key(
            script.to_str().unwrap(),
            "env://COSIGN_KEY",
            Some(&env),
            &out,
        )
        .expect("derivation succeeds");
        let argv = std::fs::read_to_string(&record).expect("read recorded argv");
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(
            lines,
            vec![
                "public-key",
                "--key=env://COSIGN_KEY",
                "--outfile",
                out.to_str().unwrap(),
            ]
        );
    }

    /// A non-zero exit from the derivation must surface the tool's stderr.
    #[cfg(unix)]
    #[test]
    fn derive_public_key_failure_carries_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("cosign");
        std::fs::write(&script, "#!/bin/sh\necho 'bad key material' >&2\nexit 1\n")
            .expect("write script");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");

        let out = tmp.path().join("derived.pub");
        let err = derive_cosign_public_key(script.to_str().unwrap(), "env://K", None, &out)
            .expect_err("must fail");
        assert!(
            format!("{err:#}").contains("bad key material"),
            "error must carry stderr: {err:#}"
        );
    }
}
