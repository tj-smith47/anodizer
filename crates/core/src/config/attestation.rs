use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// AttestationConfig
// ---------------------------------------------------------------------------

/// SLSA build-provenance / attestation configuration for binaries and archives.
///
/// Two modes select how anodizer participates in attestation:
///
/// - [`AttestationMode::Subjects`] (the default) emits a **subjects manifest**
///   (`dist/attestation-subjects.json`) that `anodizer-action` feeds to
///   GitHub's `actions/attest-build-provenance`. anodizer does NOT mint a
///   GitHub-trusted attestation itself in this mode — the Action's OIDC
///   identity does. This is the path fd / biome / gping use.
/// - [`AttestationMode::Emit`] generates a self-contained in-toto v1 statement
///   carrying an SLSA provenance v1 predicate over the selected artifacts,
///   writes it as a release asset (`attestation.intoto.jsonl`), and lets the
///   existing `signs:` stage sign it (keyed, not OIDC). This is for users who
///   can't run the Action (GoReleaser Pro `--with-provenance` parity).
///
/// YAML:
/// ```yaml
/// attestations:
///   enabled: true
///   mode: subjects          # or: emit ; default = subjects
///   artifacts: [archive, binary, checksum]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AttestationConfig {
    /// Enable attestation. When false (the default), the stage is a no-op.
    pub enabled: bool,
    /// Participation mode: `subjects` (default) writes a manifest for
    /// `actions/attest-build-provenance`; `emit` generates and signs an
    /// in-toto SLSA provenance statement as a release asset.
    pub mode: Option<AttestationMode>,
    /// Which produced-artifact kinds to attest. Each entry selects a KIND
    /// (`archive`, `binary`, `checksum`); the concrete subject set (filenames
    /// + sha256) is DERIVED from the artifacts anodizer already produced.
    ///
    /// Defaults to `[archive, binary, checksum]` when omitted.
    pub artifacts: Option<Vec<AttestationArtifactKind>>,
    /// Skip the attestation stage. Accepts a bool or a template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

/// Attestation participation mode. See [`AttestationConfig`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttestationMode {
    /// Emit a subjects manifest for `actions/attest-build-provenance` (OIDC).
    Subjects,
    /// Generate + sign a self-contained in-toto SLSA provenance statement.
    Emit,
}

/// A selectable artifact KIND for attestation. Maps to one or more concrete
/// [`crate::artifact::ArtifactKind`] values at subject-collection time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AttestationArtifactKind {
    /// Packaged archives (`.tar.gz`, `.zip`, ...) and self-extracting archives.
    Archive,
    /// Raw uploadable binaries (uploaded as bare release assets).
    Binary,
    /// Checksum file(s) (`checksums.txt` and split sidecars).
    Checksum,
}

impl AttestationConfig {
    /// Default `artifacts` selection when the user omits the field.
    pub const DEFAULT_ARTIFACTS: &'static [AttestationArtifactKind] = &[
        AttestationArtifactKind::Archive,
        AttestationArtifactKind::Binary,
        AttestationArtifactKind::Checksum,
    ];

    /// Filename of the subjects manifest written in `subjects` mode (single
    /// crate / lockstep). Per-crate workspace mode prefixes the crate name.
    pub const SUBJECTS_MANIFEST_NAME: &'static str = "attestation-subjects.json";

    /// Filename of the in-toto statement written in `emit` mode (single crate
    /// / lockstep). Per-crate workspace mode prefixes the crate name.
    pub const STATEMENT_NAME: &'static str = "attestation.intoto.jsonl";

    /// Resolve the participation mode, defaulting to `subjects`.
    pub fn resolved_mode(&self) -> AttestationMode {
        self.mode.unwrap_or(AttestationMode::Subjects)
    }

    /// Resolve the selected artifact kinds, defaulting to
    /// `[archive, binary, checksum]`.
    pub fn resolved_artifacts(&self) -> Vec<AttestationArtifactKind> {
        self.artifacts
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_ARTIFACTS.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_subjects() {
        let cfg = AttestationConfig::default();
        assert_eq!(cfg.resolved_mode(), AttestationMode::Subjects);
    }

    #[test]
    fn default_artifacts_cover_archive_binary_checksum() {
        let cfg = AttestationConfig::default();
        let kinds = cfg.resolved_artifacts();
        assert_eq!(
            kinds,
            vec![
                AttestationArtifactKind::Archive,
                AttestationArtifactKind::Binary,
                AttestationArtifactKind::Checksum,
            ]
        );
    }

    #[test]
    fn default_is_disabled() {
        assert!(!AttestationConfig::default().enabled);
    }

    #[test]
    fn parses_yaml_with_explicit_mode_and_artifacts() {
        let yaml = "enabled: true\nmode: emit\nartifacts: [archive, binary]\n";
        let cfg: AttestationConfig = serde_yaml_ng::from_str(yaml).expect("parse");
        assert!(cfg.enabled);
        assert_eq!(cfg.resolved_mode(), AttestationMode::Emit);
        assert_eq!(
            cfg.resolved_artifacts(),
            vec![
                AttestationArtifactKind::Archive,
                AttestationArtifactKind::Binary
            ]
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let yaml = "enabled: true\nbogus: 1\n";
        assert!(serde_yaml_ng::from_str::<AttestationConfig>(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_mode() {
        let yaml = "enabled: true\nmode: sideways\n";
        assert!(serde_yaml_ng::from_str::<AttestationConfig>(yaml).is_err());
    }
}
