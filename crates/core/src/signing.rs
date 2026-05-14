//! Sign / docker-sign config types.
//!
//! Lifted out of the monolithic `crate::config` module per the WAVE 5
//! split (see `.claude/known-bugs.md`'s "WAVE 5 deferred" entry). The
//! historical `anodizer_core::config::{SignConfig, DockerSignConfig}`
//! import path is preserved by re-exports at the bottom of `config.rs`.
//!
//! ## Default-resolution policy
//!
//! Both [`SignConfig`] and [`DockerSignConfig`] keep their fields as
//! `Option<T>` so the schema can distinguish "user set this explicitly"
//! from "user left it default" (preserves YAML round-trip identity and
//! lets a future override-resolution step inject values without losing
//! provenance). Stages MUST read defaults through the `resolved_*()`
//! accessors below — no inline `unwrap_or_else(|| "cosign".to_string())`
//! at call sites — so the answer to "what's the default?" lives in one
//! place per stage and a future default change (or override resolution)
//! lands in one place too. This is the lazy-vs-eager defaults policy
//! settled in Session C; precedent commit `ff3be47` (stage-checksum).

use crate::config::{StringOrBool, deserialize_string_or_bool_opt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// gpg --faked-system-time capability probe (release-resilience Task 25)
// ---------------------------------------------------------------------------

/// Probe whether the local `gpg` binary supports `--faked-system-time`.
///
/// `--faked-system-time <epoch>!` is the documented way to make gpg emit
/// a signature with a deterministic timestamp. Older gpg builds (and
/// some macOS packagers) do not support it. We probe by invoking
/// `gpg --faked-system-time 0! --version`; exit 0 means supported,
/// anything else (including gpg-not-on-PATH) means unsupported.
///
/// The preflight stage calls this once at pipeline start. When it
/// returns `false` AND the config has gpg signing configured, the
/// preflight stage adds a compile-time allow-list entry for
/// `gpg-signature.asc` so the determinism harness excludes gpg
/// signatures from drift detection, and emits a warning.
pub fn gpg_supports_faked_system_time() -> bool {
    gpg_supports_faked_system_time_with(|args| {
        std::process::Command::new("gpg").args(args).output()
    })
}

/// Probe with an injected command runner. Production code calls the
/// public `gpg_supports_faked_system_time()` wrapper; tests pass a
/// closure that returns a canned `std::process::Output` (or an io
/// error). Exposed (not `cfg(test)`) so dependent crates' tests can
/// reuse the seam without compiling stage-sign in test config.
pub fn gpg_supports_faked_system_time_with<F>(probe: F) -> bool
where
    F: FnOnce(&[&str]) -> std::io::Result<std::process::Output>,
{
    match probe(&["--faked-system-time", "0!", "--version"]) {
        Ok(out) => out.status.success(),
        Err(_) => false, // gpg not on PATH or transient io error
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SignConfig {
    /// Unique identifier for this sign config.
    pub id: Option<String>,
    /// Artifact types to sign: "all", "archive", "binary", "checksum", "package", "sbom" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign" or "gpg").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates with ${artifact} and ${signature}).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Build IDs filter: only sign artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Environment variables passed to the signing command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Capture and log stdout/stderr of the signing command.
    /// Accepts bool or template string (e.g., "{{ .IsSnapshot }}").
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub output: Option<StringOrBool>,
    /// Template-conditional: skip this sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

impl SignConfig {
    /// Default `id` when a sign config has none. Mirrors GoReleaser
    /// `internal/pipe/sign/sign.go` (`cfg.ID = "default"`). Used to
    /// label log lines and uniqueness-error messages.
    pub const DEFAULT_ID: &'static str = "default";

    /// Default `artifacts` filter for top-level `signs:[]`. Mirrors
    /// GoReleaser `sign.go` (`cfg.Artifacts = "none"`) — by default
    /// nothing is signed unless the user opts in.
    pub const DEFAULT_ARTIFACTS: &'static str = "none";

    /// Default `artifacts` filter for `binary_signs:[]`. The binary-only
    /// driver always restricts the artifact-kind filter to binaries even
    /// when the user leaves `artifacts:` unset. Anodize-specific helper
    /// (no GoReleaser equivalent — GR uses a different config type for
    /// binary signing) but kept on `SignConfig` because anodize unifies
    /// `signs[]` and `binary_signs[]` into one struct.
    pub const DEFAULT_ARTIFACTS_BINARY: &'static str = "binary";

    /// Default `signature` template for top-level `signs:[]`. Mirrors
    /// GoReleaser `sign.go` (`cfg.Signature = "${artifact}.sig"`).
    /// Anodize uses Tera-style `{{ .Artifact }}` placeholders that the
    /// arg-resolver rewrites to the same path at execution time.
    pub const DEFAULT_SIGNATURE_TEMPLATE: &'static str = "{{ .Artifact }}.sig";

    /// Default `signature` template for `binary_signs:[]`.
    ///
    /// Intentionally **diverges from GoReleaser** `sign_binary.go:16`: GR
    /// stores binaries under per-target subdirectories
    /// (`dist/linux_amd64/binname`), so its template appends `_{{ .Os }}_{{ .Arch }}`
    /// to the bare binary name without collision. Anodize uses a flat `dist/`
    /// layout where stage-build already names binaries with the platform
    /// suffix (`myapp_linux_amd64`, `myapp_darwin_arm64`, etc.). Appending
    /// Os/Arch again would produce `myapp_linux_amd64_linux_amd64` with no
    /// `.sig` extension — a double-suffix bug.
    ///
    /// The correct default for anodize's layout is `{{ .Artifact }}.sig` —
    /// identical to `DEFAULT_SIGNATURE_TEMPLATE`. Binary names are already
    /// unique per target, so no collision risk exists. Users who want an
    /// explicit per-target suffix can set `signature:` in `binary_signs:`.
    pub const DEFAULT_BINARY_SIGNATURE_TEMPLATE: &'static str = "{{ .Artifact }}.sig";

    /// Default `args` for top-level `signs:[]`. Mirrors GoReleaser
    /// `sign.go` (`["--output", "$signature", "--detach-sig", "$artifact"]`).
    /// Anodize substitutes `$signature` / `$artifact` for `{{ .Signature }}`
    /// / `{{ .Artifact }}` Tera placeholders that the arg-resolver
    /// rewrites; the wire-level invocation matches GR exactly.
    pub const DEFAULT_ARGS: &[&'static str] = &[
        "--output",
        "{{ .Signature }}",
        "--detach-sig",
        "{{ .Artifact }}",
    ];

    /// Resolve the sign-config id, falling back to `"default"` (GoReleaser-canonical).
    pub fn resolved_id(&self) -> &str {
        self.id.as_deref().unwrap_or(Self::DEFAULT_ID)
    }

    /// Resolve the `artifacts` filter, falling back to the supplied
    /// `fallback` (`Self::DEFAULT_ARTIFACTS` for `signs[]`,
    /// `Self::DEFAULT_ARTIFACTS_BINARY` for `binary_signs[]`).
    pub fn resolved_artifacts<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.artifacts.as_deref().unwrap_or(fallback)
    }

    /// Resolve the `signature` template, falling back to the supplied
    /// `default` (`Self::DEFAULT_SIGNATURE_TEMPLATE` for `signs[]`,
    /// `Self::DEFAULT_BINARY_SIGNATURE_TEMPLATE` for `binary_signs[]`).
    pub fn resolved_signature_template<'a>(&'a self, default: &'a str) -> &'a str {
        self.signature.as_deref().unwrap_or(default)
    }

    /// Resolve `args`, materializing the [`Self::DEFAULT_ARGS`] const into
    /// a `Vec<String>` when the user left `args:` unset. Returns a clone
    /// of the user-supplied list otherwise.
    pub fn resolved_args(&self) -> Vec<String> {
        self.args.clone().unwrap_or_else(|| {
            Self::DEFAULT_ARGS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        })
    }

    /// `true` when this sign config will invoke gpg.
    ///
    /// The top-level `signs:` driver defaults to gpg when `cmd:` is unset
    /// (see `stage-sign::helpers::default_sign_cmd` which falls back to
    /// `git config gpg.program` then to literal `"gpg"`). We treat any
    /// cmd whose basename starts with `gpg` (e.g., `gpg`, `gpg2`,
    /// `/usr/local/bin/gpg`) as a gpg invocation. A cmd of `"cosign"`,
    /// `"notation"`, etc. returns false.
    ///
    /// Entries with `artifacts: "none"` (the default for top-level
    /// `signs:`) are treated as not-configured — the loop never fires.
    pub fn is_gpg(&self) -> bool {
        // Effectively-disabled entries don't count as configured.
        let artifacts = self.resolved_artifacts(Self::DEFAULT_ARTIFACTS);
        if artifacts == "none" {
            return false;
        }
        match self.cmd.as_deref() {
            None => true, // default cmd is gpg
            Some(cmd) => {
                let basename = std::path::Path::new(cmd)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(cmd);
                basename.starts_with("gpg")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerSignConfig {
    /// Unique identifier for this docker sign config.
    pub id: Option<String>,
    /// Docker artifact types to sign: "all", "image", or "manifest" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Docker config IDs filter: only sign images from configs whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Environment variables passed to the signing command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Capture and log stdout/stderr of the docker signing command.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub output: Option<StringOrBool>,
    /// Template-conditional: skip this docker sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

impl DockerSignConfig {
    /// Default `id` when a docker-sign config has none. Mirrors GoReleaser
    /// `internal/pipe/sign/sign_docker.go` (`cfg.ID = "default"`).
    pub const DEFAULT_ID: &'static str = "default";

    /// Default signing `cmd`. Mirrors GoReleaser `sign_docker.go`
    /// (`cfg.Cmd = "cosign"`). Unlike top-level `signs:[]` (which falls
    /// back to git's `gpg.program` config), docker signing only ever
    /// targets cosign, so the default is a static literal.
    pub const DEFAULT_CMD: &'static str = "cosign";

    /// Default `artifacts` filter when unset. Empty string is treated by
    /// the docker-sign driver as "DockerImageV2 only" (post-buildx
    /// canonical case). Mirrors GR's lack of an explicit fallback —
    /// GR's switch on `cfg.Artifacts` treats `""` identically.
    pub const DEFAULT_ARTIFACTS: &'static str = "";

    /// Default `args` for `docker_signs:[]`. Mirrors GoReleaser
    /// `sign_docker.go` (`["sign", "--key=cosign.key",
    /// "${artifact}@${digest}", "--yes"]`). Anodize substitutes
    /// `${artifact}@${digest}` for the Tera-rewritten
    /// `{{ .Artifact }}@{{ .Digest }}` placeholders.
    pub const DEFAULT_ARGS: &[&'static str] = &[
        "sign",
        "--key=cosign.key",
        "{{ .Artifact }}@{{ .Digest }}",
        "--yes",
    ];

    /// Resolve the docker-sign id, falling back to `"default"` (GR-canonical).
    pub fn resolved_id(&self) -> &str {
        self.id.as_deref().unwrap_or(Self::DEFAULT_ID)
    }

    /// Resolve the signing command, falling back to `"cosign"` (GR-canonical).
    pub fn resolved_cmd(&self) -> &str {
        self.cmd.as_deref().unwrap_or(Self::DEFAULT_CMD)
    }

    /// Resolve the `artifacts` filter, falling back to `""` (DockerImageV2 only).
    pub fn resolved_artifacts(&self) -> &str {
        self.artifacts.as_deref().unwrap_or(Self::DEFAULT_ARTIFACTS)
    }

    /// Resolve `args`, materializing the [`Self::DEFAULT_ARGS`] const into
    /// a `Vec<String>` when the user left `args:` unset.
    pub fn resolved_args(&self) -> Vec<String> {
        self.args.clone().unwrap_or_else(|| {
            Self::DEFAULT_ARGS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SignConfig::resolved_*() (Session C lazy-defaults policy) ----

    #[test]
    fn sign_resolved_id_default() {
        assert_eq!(SignConfig::default().resolved_id(), "default");
    }

    #[test]
    fn sign_resolved_id_user_value_wins() {
        let cfg = SignConfig {
            id: Some("cosign".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_id(), "cosign");
    }

    #[test]
    fn sign_resolved_artifacts_falls_back_to_supplied_default() {
        let cfg = SignConfig::default();
        assert_eq!(
            cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS),
            "none"
        );
        assert_eq!(
            cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS_BINARY),
            "binary"
        );
    }

    #[test]
    fn sign_resolved_artifacts_user_value_wins_over_fallback() {
        let cfg = SignConfig {
            artifacts: Some("checksum".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS),
            "checksum"
        );
        assert_eq!(
            cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS_BINARY),
            "checksum"
        );
    }

    #[test]
    fn sign_resolved_signature_template_default_paths() {
        let cfg = SignConfig::default();
        assert_eq!(
            cfg.resolved_signature_template(SignConfig::DEFAULT_SIGNATURE_TEMPLATE),
            "{{ .Artifact }}.sig"
        );
        // Binary default now equals the simple .sig template — flat layout means
        // binary names already carry the platform suffix.
        assert_eq!(
            cfg.resolved_signature_template(SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE),
            "{{ .Artifact }}.sig"
        );
    }

    #[test]
    fn sign_resolved_signature_template_user_value_wins() {
        let cfg = SignConfig {
            signature: Some("custom-{{ .Artifact }}.asc".to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_signature_template(SignConfig::DEFAULT_SIGNATURE_TEMPLATE),
            "custom-{{ .Artifact }}.asc"
        );
    }

    #[test]
    fn sign_resolved_args_default_matches_goreleaser() {
        let cfg = SignConfig::default();
        assert_eq!(
            cfg.resolved_args(),
            vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sig".to_string(),
                "{{ .Artifact }}".to_string(),
            ]
        );
    }

    #[test]
    fn sign_resolved_args_user_value_wins() {
        let custom = vec!["sign".to_string(), "--key=k".to_string()];
        let cfg = SignConfig {
            args: Some(custom.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_args(), custom);
    }

    // ---- DockerSignConfig::resolved_*() ----

    #[test]
    fn docker_sign_resolved_id_default() {
        assert_eq!(DockerSignConfig::default().resolved_id(), "default");
    }

    #[test]
    fn docker_sign_resolved_id_user_value_wins() {
        let cfg = DockerSignConfig {
            id: Some("custom".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_id(), "custom");
    }

    #[test]
    fn docker_sign_resolved_cmd_default() {
        assert_eq!(DockerSignConfig::default().resolved_cmd(), "cosign");
    }

    #[test]
    fn docker_sign_resolved_cmd_user_value_wins() {
        let cfg = DockerSignConfig {
            cmd: Some("notation".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_cmd(), "notation");
    }

    #[test]
    fn docker_sign_resolved_artifacts_default() {
        assert_eq!(DockerSignConfig::default().resolved_artifacts(), "");
    }

    #[test]
    fn docker_sign_resolved_artifacts_user_value_wins() {
        let cfg = DockerSignConfig {
            artifacts: Some("manifests".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_artifacts(), "manifests");
    }

    #[test]
    fn docker_sign_resolved_args_default_matches_goreleaser() {
        assert_eq!(
            DockerSignConfig::default().resolved_args(),
            vec![
                "sign".to_string(),
                "--key=cosign.key".to_string(),
                "{{ .Artifact }}@{{ .Digest }}".to_string(),
                "--yes".to_string(),
            ]
        );
    }

    #[test]
    fn docker_sign_resolved_args_user_value_wins() {
        let custom = vec!["verify".to_string(), "--cert=c".to_string()];
        let cfg = DockerSignConfig {
            args: Some(custom.clone()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_args(), custom);
    }

    // ---- gpg --faked-system-time capability probe (Task 25) ----------

    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    fn mk_output(success: bool) -> Output {
        let status = if success {
            ExitStatus::from_raw(0)
        } else {
            // non-zero exit
            ExitStatus::from_raw(1 << 8)
        };
        Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn gpg_faked_time_supported_returns_true_when_probe_succeeds() {
        let supported = gpg_supports_faked_system_time_with(|args| {
            assert_eq!(args, &["--faked-system-time", "0!", "--version"]);
            Ok(mk_output(true))
        });
        assert!(supported);
    }

    #[test]
    fn gpg_faked_time_unsupported_returns_false_when_probe_fails() {
        let supported = gpg_supports_faked_system_time_with(|_| Ok(mk_output(false)));
        assert!(!supported);
    }

    #[test]
    fn gpg_faked_time_returns_false_when_probe_errors() {
        let supported = gpg_supports_faked_system_time_with(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gpg not on PATH",
            ))
        });
        assert!(!supported);
    }

    // ---- SignConfig::is_gpg() ---------------------------------------

    #[test]
    fn is_gpg_default_cmd_with_signing_artifacts_is_true() {
        // No cmd set + artifacts set to something other than "none" =
        // default gpg invocation, treated as gpg-configured.
        let cfg = SignConfig {
            artifacts: Some("all".to_string()),
            ..Default::default()
        };
        assert!(cfg.is_gpg());
    }

    #[test]
    fn is_gpg_default_artifacts_none_is_false() {
        // Default artifacts filter is "none" — entry is effectively
        // disabled, so it does not count as gpg-configured.
        let cfg = SignConfig::default();
        assert!(!cfg.is_gpg());
    }

    #[test]
    fn is_gpg_cosign_cmd_is_false() {
        let cfg = SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        };
        assert!(!cfg.is_gpg());
    }

    #[test]
    fn is_gpg_gpg2_cmd_is_true() {
        let cfg = SignConfig {
            artifacts: Some("checksum".to_string()),
            cmd: Some("gpg2".to_string()),
            ..Default::default()
        };
        assert!(cfg.is_gpg());
    }

    #[test]
    fn is_gpg_absolute_gpg_path_is_true() {
        let cfg = SignConfig {
            artifacts: Some("binary".to_string()),
            cmd: Some("/usr/local/bin/gpg".to_string()),
            ..Default::default()
        };
        assert!(cfg.is_gpg());
    }
}
