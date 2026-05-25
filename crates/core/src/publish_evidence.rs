//! Per-publisher run evidence (the `evidence.json` shape).
//!
//! [`PublishEvidence`] captures what a publisher actually pushed plus
//! the operator-public coordinates a later `--rollback-only --from-run`
//! consumes. The [`extra`] slot used to be a free-form
//! `serde_json::Value`; it is now a typed enum
//! ([`PublishEvidenceExtra`]) so the type system structurally
//! prevents credential leakage — a publisher cannot serialize a
//! credential-shaped field into evidence because the variant struct
//! has no such field to hold it.
//!
//! Wire format is preserved: `#[serde(untagged)]` on the enum keeps
//! the rendered JSON identical to the prior free-form
//! `{ "<publisher>_targets": [...] }` shape, so consumers of
//! `dist/run-<id>/report.json` and `summary.json` see the same bytes.
//!
//! [`extra`]: PublishEvidence::extra

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One entry in [`HomebrewExtra::homebrew_targets`] — the operator-public
/// snapshot of a single tap push. Mirrors the serialized field set of
/// `HomebrewTarget` in `stage-publish/src/homebrew/publisher.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewTargetSnapshot {
    /// Per-target label — formula name, cask name, or `homebrew_casks`
    /// for the top-level path.
    pub target: String,
    /// HTTPS clone URL of the tap repo.
    pub repo_url: String,
    /// Branch the publish path pushed to. `None` means default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Env var NAME to consult for the rollback re-clone token.
    /// NEVER the token VALUE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HomebrewExtra {
    pub homebrew_targets: Vec<HomebrewTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScoopTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ScoopExtra {
    pub scoop_targets: Vec<ScoopTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NixTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NixExtra {
    pub nix_targets: Vec<NixTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WingetTargetSnapshot {
    pub target: String,
    pub crate_name: String,
    pub package_id: String,
    pub version: String,
    pub upstream_owner: String,
    pub upstream_repo: String,
    pub fork_owner: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WingetExtra {
    pub winget_targets: Vec<WingetTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChocolateyTargetSnapshot {
    pub target: String,
    pub crate_name: String,
    pub package_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChocolateyExtra {
    pub chocolatey_targets: Vec<ChocolateyTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KrewTargetSnapshot {
    pub target: String,
    pub upstream_owner: String,
    pub upstream_repo: String,
    pub fork_owner: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KrewExtra {
    pub krew_targets: Vec<KrewTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurTargetSnapshot {
    pub target: String,
    /// AUR SSH URL — operator-public coordinate.
    pub git_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurExtra {
    pub aur_our_targets: Vec<AurTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurSourceTargetSnapshot {
    pub target: String,
    pub package: String,
    pub tag: String,
    pub git_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AurSourceExtra {
    pub aur_source_targets: Vec<AurSourceTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct McpTargetSnapshot {
    pub target: String,
    pub server_name: String,
    pub registry_url: String,
    pub version: String,
    /// MCP auth method — operator-public; carries no credential bytes.
    /// Serializes as `"none"` / `"github"` / `"github-oidc"` per the
    /// rename annotations on [`crate::config::McpAuthMethod`].
    pub auth_method: crate::config::McpAuthMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct McpExtra {
    pub mcp_targets: Vec<McpTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DockerhubTargetSnapshot {
    pub target: String,
    pub repo_url: String,
    pub namespace: String,
    pub name: String,
    /// DockerHub login — operator-public.
    pub username: String,
    /// Env var NAME the rollback path consults to re-resolve the password.
    /// NEVER the password VALUE.
    pub secret_env_var: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_full_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DockerhubExtra {
    pub dockerhub_targets: Vec<DockerhubTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ArtifactoryTargetSnapshot {
    pub entry: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ArtifactoryExtra {
    pub artifactory_targets: Vec<ArtifactoryTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudsmithTargetSnapshot {
    pub org: String,
    pub repo: String,
    pub filename: String,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudsmithExtra {
    pub cloudsmith_targets: Vec<CloudsmithTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BlobTargetSnapshot {
    pub provider: String,
    pub bucket: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BlobExtra {
    pub blob_targets: Vec<BlobTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SnapcraftTargetSnapshot {
    pub crate_name: String,
    pub package_name: String,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SnapcraftExtra {
    pub snapcraft_targets: Vec<SnapcraftTargetSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubReleaseTargetSnapshot {
    pub crate_name: String,
    pub owner: String,
    pub repo: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubReleaseExtra {
    pub github_release_targets: Vec<GithubReleaseTargetSnapshot>,
}

/// Typed `extra` payload for [`PublishEvidence`]. Untagged on the wire —
/// each variant's JSON shape matches the prior free-form
/// `serde_json::json!({"<publisher>_targets": [...]})` form so existing
/// consumers of `dist/run-<id>/report.json` and `summary.json` see no
/// byte-shape change.
///
/// **CREDENTIAL CONTRACT**: every variant's inner struct exposes ONLY
/// operator-public fields. Credential VALUES (token bytes, passwords,
/// SSH key material) have no field to land in — the type system rejects
/// any future leak attempt at the compile boundary. Per-publisher
/// runtime credentials (resolved from env / config at publish time)
/// live in crate-local `*Target` structs with `#[serde(skip)]`
/// discipline; they convert into the snapshots above at the encode
/// boundary, dropping the secret fields by definition.
///
/// The [`Empty`](Self::Empty) variant covers publishers that have no
/// per-evidence operator-public fields (or that no-op'd the run).
/// Serializes as `null` on the wire and is the deserialization
/// fallback for the same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum PublishEvidenceExtra {
    Homebrew(HomebrewExtra),
    Scoop(ScoopExtra),
    Nix(NixExtra),
    Winget(WingetExtra),
    Chocolatey(ChocolateyExtra),
    Krew(KrewExtra),
    Aur(AurExtra),
    AurSource(AurSourceExtra),
    Mcp(McpExtra),
    Dockerhub(DockerhubExtra),
    Artifactory(ArtifactoryExtra),
    Cloudsmith(CloudsmithExtra),
    Blob(BlobExtra),
    Snapcraft(SnapcraftExtra),
    GithubRelease(GithubReleaseExtra),
    /// Default for publishers with no per-evidence operator-public fields,
    /// or for runs that no-op'd. Serializes as JSON `null`.
    #[default]
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishEvidence {
    pub schema_version: u32,
    pub publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_ref: Option<String>,
    pub artifact_paths: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nondeterministic: Option<String>,
    /// Operator-public metadata for the publisher run.
    ///
    /// **CREDENTIAL CONTRACT**: this field is persisted to
    /// `dist/run-<id>/report.json`, summarised in `summary.json`, and
    /// may be attached to the GitHub Release body via the announce
    /// stage. It carries only operator-public identifiers (URLs,
    /// env-var NAMES, PR numbers, tag strings, branch names). Token
    /// VALUES, private keys, passwords, OAuth secrets, SSH key
    /// material have no variant field to land in — the
    /// [`PublishEvidenceExtra`] enum's per-variant struct list is the
    /// schema, and serde rejects fields it does not name.
    ///
    /// Per-publisher rollback state (runtime-only credentials read
    /// from env / config at publish time) lives in crate-local
    /// `*Target` structs with `#[serde(skip)]` discipline; those
    /// convert into the [`PublishEvidenceExtra`] variant snapshots at
    /// the encode boundary, dropping the secret fields by definition.
    #[serde(default)]
    pub extra: PublishEvidenceExtra,
}

impl PublishEvidence {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn new(publisher: impl Into<String>) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            publisher: publisher.into(),
            primary_ref: None,
            artifact_paths: Vec::new(),
            nondeterministic: None,
            extra: PublishEvidenceExtra::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_evidence_roundtrips_through_json() {
        let mut e = PublishEvidence::new("homebrew");
        e.primary_ref = Some("refs/heads/main".to_string());
        e.artifact_paths.push(PathBuf::from("dist/foo.tar.gz"));
        e.nondeterministic = Some("timestamp".to_string());
        e.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
            homebrew_targets: vec![HomebrewTargetSnapshot {
                target: "demo".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            }],
        });

        let s = serde_json::to_string(&e).expect("serialize");
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_omits_none_fields_on_serialize() {
        let e = PublishEvidence::new("homebrew");
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(
            !s.contains("primary_ref"),
            "primary_ref should be omitted when None: {s}"
        );
        assert!(
            !s.contains("nondeterministic"),
            "nondeterministic should be omitted when None: {s}"
        );
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_rejects_unknown_fields() {
        let bad = r#"{
            "schema_version": 1,
            "publisher": "homebrew",
            "primary_ref": null,
            "artifact_paths": [],
            "nondeterministic": null,
            "extra": null,
            "future_field": "boom"
        }"#;
        let r: Result<PublishEvidence, _> = serde_json::from_str(bad);
        assert!(r.is_err(), "deny_unknown_fields should reject future_field");
    }

    #[test]
    fn empty_variant_serializes_as_null() {
        // The Empty variant is the default for newly constructed evidence;
        // pinning its wire shape ensures back-compat with the prior `{}` /
        // null default and avoids accidental shape drift.
        let e = PublishEvidence::new("homebrew");
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        assert_eq!(v["extra"], serde_json::Value::Null);
    }

    #[test]
    fn empty_variant_deserializes_from_null() {
        // Untagged enum: null lands on Empty (the unit variant is the
        // only one that accepts a null payload). Pin the wire shape
        // so a future variant addition that breaks this path fails
        // here.
        let from_null = serde_json::from_str::<PublishEvidenceExtra>("null").expect("null");
        assert_eq!(from_null, PublishEvidenceExtra::Empty);
    }

    #[test]
    fn publish_evidence_extra_json_shape_matches_pre_typed_form() {
        // Wire-format pin: downstream consumers of
        // `dist/run-<id>/report.json` see the same byte shape that
        // shipped pre-typed-enum. A variant addition that drifts the
        // shape (e.g. wraps the homebrew_targets array in an extra
        // object) fails this test.
        let e = PublishEvidence {
            extra: PublishEvidenceExtra::Homebrew(HomebrewExtra {
                homebrew_targets: vec![HomebrewTargetSnapshot {
                    target: "demo".into(),
                    repo_url: "https://github.com/owner/tap".into(),
                    branch: Some("anodize-update".into()),
                    token_env_var: Some("ANODIZER_GITHUB_TOKEN".into()),
                }],
            }),
            ..PublishEvidence::new("homebrew")
        };
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        let t = &v["extra"]["homebrew_targets"][0];
        assert_eq!(t["target"], "demo");
        assert_eq!(t["repo_url"], "https://github.com/owner/tap");
        assert_eq!(t["branch"], "anodize-update");
        assert_eq!(t["token_env_var"], "ANODIZER_GITHUB_TOKEN");
        // Defense-in-depth: no credential-shaped keys in the rendered
        // form (matches the per-publisher `*_extra_carries_no_secret_material`
        // tests but pinned at the core wire-format level).
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
    }
}
