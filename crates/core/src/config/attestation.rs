use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};
use crate::artifact::{ArtifactKind, release_uploadable_kinds};

// AttestationConfig

/// SLSA build-provenance / attestation configuration for binaries and archives.
///
/// Two modes select how anodizer participates in attestation:
///
/// - `mode: subjects` (the default) writes a **subjects manifest**
///   (`dist/attestation-subjects.json`) that `anodizer-action` passes to
///   GitHub's `actions/attest-build-provenance`. In this mode anodizer creates
///   no GitHub-trusted attestation itself; the Action creates it with its
///   OIDC identity. fd, biome and gping attest this way.
/// - `mode: emit` generates a self-contained in-toto v1 statement
///   carrying an SLSA provenance v1 predicate over the selected artifacts,
///   writes it as a release asset (`attestation.intoto.jsonl`), and lets the
///   `signs:` configuration sign it with a key. This is for users who
///   cannot run the Action.
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
    /// Which produced-artifact kinds to attest. Each entry selects a kind;
    /// the concrete subjects (file names and sha256) come from the
    /// artifacts anodizer produced. The kinds are `archive`, `binary`,
    /// `checksum`, `package`, `source`, `sbom`, `installer`, `flatpak`,
    /// `install_script` and `file`.
    ///
    /// When omitted, every artifact uploaded to the release is attested
    /// except signatures and certificates, which are themselves signatures
    /// over other artifacts. That is the same set all ten kinds select
    /// together.
    pub artifacts: Option<Vec<AttestationArtifactKind>>,
    /// Skip the attestation stage. Accepts a bool or a template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

/// Attestation mode: `subjects` or `emit`. See the `attestations` section.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttestationMode {
    /// Emit a subjects manifest for `actions/attest-build-provenance` (OIDC).
    Subjects,
    /// Generate + sign a self-contained in-toto SLSA provenance statement.
    Emit,
}

/// A kind of artifact to attest. Each kind covers one or more of the
/// artifact types anodizer produces. Together they cover every artifact
/// uploaded to a release except signatures and certificates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AttestationArtifactKind {
    /// Packaged archives (`.tar.gz`, `.zip`, ...) and self-extracting archives.
    Archive,
    /// Raw uploadable binaries (uploaded as bare release assets).
    Binary,
    /// Checksum files (`checksums.txt` and split sidecars).
    Checksum,
    /// Linux packages (`.deb` / `.rpm` / `.apk`) and source RPMs.
    Package,
    /// Source archives (`source:` tarball).
    Source,
    /// Generated SBOM documents.
    Sbom,
    /// OS installers: Windows MSI/NSIS, macOS DMG (disk image), and macOS PKG.
    Installer,
    /// Flatpak bundles (`.flatpak`).
    Flatpak,
    /// Generated install scripts (`install.sh`, `install.ps1`).
    InstallScript,
    /// Other uploadable files: rendered `template_files` outputs and AppImage
    /// `.zsync` update files.
    File,
}

impl AttestationArtifactKind {
    /// Every selectable kind, in the order the `artifacts` doc lists them.
    pub const ALL: [AttestationArtifactKind; 10] = [
        AttestationArtifactKind::Archive,
        AttestationArtifactKind::Binary,
        AttestationArtifactKind::Checksum,
        AttestationArtifactKind::Package,
        AttestationArtifactKind::Source,
        AttestationArtifactKind::Sbom,
        AttestationArtifactKind::Installer,
        AttestationArtifactKind::Flatpak,
        AttestationArtifactKind::InstallScript,
        AttestationArtifactKind::File,
    ];

    /// The concrete artifact kinds this selection attests.
    pub fn concrete_kinds(self) -> &'static [ArtifactKind] {
        match self {
            // AppImage and makeself are single self-contained bundles, the
            // same shape as an archive.
            AttestationArtifactKind::Archive => &[
                ArtifactKind::Archive,
                ArtifactKind::Makeself,
                ArtifactKind::AppImage,
            ],
            // The bare-binary release asset; the intermediate build output
            // never reaches the release.
            AttestationArtifactKind::Binary => &[ArtifactKind::UploadableBinary],
            AttestationArtifactKind::Checksum => &[ArtifactKind::Checksum],
            AttestationArtifactKind::Package => {
                &[ArtifactKind::LinuxPackage, ArtifactKind::SourceRpm]
            }
            AttestationArtifactKind::Source => &[ArtifactKind::SourceArchive],
            AttestationArtifactKind::Sbom => &[ArtifactKind::Sbom],
            AttestationArtifactKind::Installer => &[
                ArtifactKind::Installer,
                ArtifactKind::DiskImage,
                ArtifactKind::MacOsPackage,
            ],
            AttestationArtifactKind::Flatpak => &[ArtifactKind::Flatpak],
            AttestationArtifactKind::InstallScript => &[ArtifactKind::InstallScript],
            AttestationArtifactKind::File => &[ArtifactKind::UploadableFile],
        }
    }
}

/// The concrete artifact kinds attested when `artifacts:` is omitted: every
/// release-uploadable kind except signatures and certificates. Derived from
/// [`release_uploadable_kinds`] so a kind added to the release is attested
/// by default.
pub fn default_attestable_kinds() -> Vec<ArtifactKind> {
    release_uploadable_kinds()
        .iter()
        .copied()
        .filter(|k| !matches!(k, ArtifactKind::Signature | ArtifactKind::Certificate))
        .collect()
}

impl AttestationConfig {
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

    /// The configured artifact-kind selection, or `None` when `artifacts:` is
    /// omitted.
    ///
    /// The stage reads `None` as "attest every release-uploadable artifact"
    /// ([`default_attestable_kinds`], less the attestation outputs
    /// themselves), so a `.deb`, `.rpm`, SBOM or installer the user ships is
    /// attested by default.
    pub fn resolved_artifacts(&self) -> Option<Vec<AttestationArtifactKind>> {
        self.artifacts.clone()
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
    fn omitted_artifacts_resolve_to_none_meaning_attest_everything() {
        // None signals the stage to attest the full release-uploadable set,
        // not a hand-curated subset.
        let cfg = AttestationConfig::default();
        assert_eq!(cfg.resolved_artifacts(), None);
    }

    #[test]
    fn parses_newly_selectable_kinds() {
        let yaml = "enabled: true\nartifacts: [package, source, sbom, installer]\n";
        let cfg: AttestationConfig = serde_yaml_ng::from_str(yaml).expect("parse");
        assert_eq!(
            cfg.resolved_artifacts(),
            Some(vec![
                AttestationArtifactKind::Package,
                AttestationArtifactKind::Source,
                AttestationArtifactKind::Sbom,
                AttestationArtifactKind::Installer,
            ])
        );
    }

    #[test]
    fn the_selectable_kinds_together_are_the_default_set() {
        let mut selectable: Vec<ArtifactKind> = AttestationArtifactKind::ALL
            .iter()
            .flat_map(|k| k.concrete_kinds())
            .copied()
            .collect();
        let mut default = default_attestable_kinds();
        selectable.sort_by_key(|k| k.as_str());
        default.sort_by_key(|k| k.as_str());
        let before = selectable.len();
        selectable.dedup();
        assert_eq!(before, selectable.len(), "two selections share a kind");
        assert_eq!(selectable, default);
    }

    #[test]
    fn the_artifacts_doc_lists_every_selectable_kind_in_order() {
        let schema = schemars::schema_for!(AttestationConfig);
        let doc = schema
            .as_value()
            .pointer("/properties/artifacts/description")
            .and_then(|v| v.as_str())
            .expect("artifacts carries a description")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let names: Vec<String> = AttestationArtifactKind::ALL
            .iter()
            .map(|k| {
                serde_json::to_value(k)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let listed = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>();
        let sentence = format!(
            "{} and {}",
            listed[..listed.len() - 1].join(", "),
            listed[listed.len() - 1]
        );
        assert!(
            doc.contains(&sentence),
            "the artifacts doc must list exactly {sentence}; got: {doc}"
        );
        // ALL holds every variant the schema publishes.
        let def = schema
            .as_value()
            .pointer("/$defs/AttestationArtifactKind")
            .expect("kind definition");
        let mut published: Vec<String> = Vec::new();
        let mut stack = vec![def];
        while let Some(v) = stack.pop() {
            if let Some(c) = v.get("const").and_then(|c| c.as_str()) {
                published.push(c.to_string());
            }
            for key in ["enum", "oneOf", "anyOf"] {
                if let Some(items) = v.get(key).and_then(|i| i.as_array()) {
                    for item in items {
                        match item.as_str() {
                            Some(s) => published.push(s.to_string()),
                            None => stack.push(item),
                        }
                    }
                }
            }
        }
        let mut sorted_names = names.clone();
        sorted_names.sort();
        published.sort();
        assert_eq!(published, sorted_names, "ALL and the enum disagree");
        // Every variant parses from the name the doc gives it.
        for (name, kind) in names.iter().zip(AttestationArtifactKind::ALL) {
            let parsed: AttestationArtifactKind =
                serde_yaml_ng::from_str(name).expect("documented kind parses");
            assert_eq!(parsed, kind);
        }
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
            Some(vec![
                AttestationArtifactKind::Archive,
                AttestationArtifactKind::Binary
            ])
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
