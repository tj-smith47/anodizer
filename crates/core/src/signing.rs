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

    /// Default `signature` template for `binary_signs:[]`. Mirrors
    /// GoReleaser `internal/pipe/sign/sign_binary.go` `defaultSignatureName`
    /// — emits a per-target filename including Os/Arch/Arm/Mips/Amd64
    /// suffixes so signatures don't collide across architectures.
    pub const DEFAULT_BINARY_SIGNATURE_TEMPLATE: &'static str = "{{ .Artifact }}_{{ Os }}_{{ Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

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
        assert_eq!(
            cfg.resolved_signature_template(SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE),
            SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE
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
}
