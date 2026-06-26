use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ContentSource, ExtraFileSpec, HumanDuration, StringOrBool, TemplatedExtraFile,
    deserialize_string_or_bool_opt,
};

// ---------------------------------------------------------------------------
// ReleaseConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReleaseConfig {
    /// GitHub repository to release to (owner and name).
    pub github: Option<ScmRepoConfig>,
    /// GitLab repository to release to (owner and name).
    pub gitlab: Option<ScmRepoConfig>,
    /// Gitea repository to release to (owner and name).
    pub gitea: Option<ScmRepoConfig>,
    /// When true, create the release as a draft (unpublished).
    pub draft: Option<bool>,
    #[schemars(schema_with = "prerelease_schema")]
    /// Mark release as pre-release: true, false, or "auto" (inferred from tag).
    pub prerelease: Option<PrereleaseConfig>,
    #[schemars(schema_with = "make_latest_schema")]
    /// Mark release as latest: true, false, or "auto" (latest non-prerelease).
    pub make_latest: Option<MakeLatestConfig>,
    /// Release title template (supports templates).
    pub name_template: Option<String>,
    /// Text prepended to the release body (inline string, from_file, or from_url).
    pub header: Option<ContentSource>,
    /// Text appended to the release body (inline string, from_file, or from_url).
    pub footer: Option<ContentSource>,
    /// Extra files to upload to the release beyond build artifacts.
    ///
    /// Paths / globs are resolved relative to the project root. `..`
    /// segments are accepted, so an entry
    /// like `../sibling/dist/*` will reach outside the project tree —
    /// security-conscious users should keep the entries inside the repo or
    /// canonicalise them before invoking the release pipeline.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Extra files whose contents are rendered through the template engine before upload.
    /// Unlike `extra_files` which copy as-is, template variables like `{{ Tag }}` are expanded.
    ///
    /// Same path-traversal caveat as `extra_files`: `..` segments reach
    /// outside the project tree.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Skip uploading artifacts: true, false, or "auto" (skip for snapshots).
    /// Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip_upload: Option<StringOrBool>,
    /// When true, replace an existing draft release instead of failing.
    pub replace_existing_draft: Option<bool>,
    /// When true, replace existing release artifacts with the same name.
    pub replace_existing_artifacts: Option<bool>,
    /// Skip the release stage. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    /// Template strings are supported here.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat
    /// with imported configs (the legacy `disable:` spelling).
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,
    /// Release mode: "keep-existing", "append", "prepend", or "replace".
    pub mode: Option<String>,
    /// Artifact IDs filter for uploads. Release-wide artifacts (checksums,
    /// source archive, extra files, metadata) always upload regardless of
    /// the filter, and derived artifacts (signatures, certificates, SBOMs)
    /// inherit the verdict of the artifact they derive from — a signature
    /// uploads iff the artifact it signs uploads.
    pub ids: Option<Vec<String>>,
    /// Target branch or SHA for the release tag.
    pub target_commitish: Option<String>,
    /// GitHub Discussion category name for the release.
    pub discussion_category_name: Option<String>,
    /// Upload metadata.json and artifacts.json as release assets.
    pub include_meta: Option<bool>,
    /// Reuse an existing draft release instead of creating a new one.
    pub use_existing_draft: Option<bool>,
    /// Override the release tag (template string). When set, this tag is used
    /// as the `tag_name` in the GitHub release API instead of the crate's
    /// `tag_template`. Useful in monorepo setups to strip a tag prefix
    /// (e.g. `"{{ Tag }}"` to publish `v1.0.0` instead of `myapp/v1.0.0`).
    /// A cross-platform publishing feature provided for free by anodizer.
    pub tag: Option<String>,
    /// Maximum number of asset-upload requests in flight simultaneously.
    ///
    /// GitHub's secondary rate-limit is triggered by burst traffic. Keeping
    /// this value low avoids tripping the limit even for releases with many
    /// artifacts. Default: 4. Override at runtime with
    /// `ANODIZER_GITHUB_UPLOAD_CONCURRENCY`.
    pub upload_concurrency: Option<u32>,
    /// Minimum interval between successive asset-upload *starts* (a humantime
    /// string, e.g. `"200ms"`, `"1s"`, `"0s"`).
    ///
    /// This is a *proactive* pace that smooths the initial burst of upload
    /// requests, layered on top of [`Self::upload_concurrency`] (the
    /// concurrency cap) and the reactive secondary-rate-limit backoff. With
    /// the concurrency cap alone, the first N uploads fire in the same instant
    /// — exactly the burst pattern that trips GitHub's secondary rate limit.
    /// Spacing each upload's *start* by this interval (with ±20% jitter so
    /// concurrent releases don't synchronise) makes the burst far less likely
    /// to trip the limit in the first place.
    ///
    /// Default: `"200ms"` — at the default concurrency of 4 this caps the
    /// initial start rate at ~5/s, which is below the burst threshold yet adds
    /// negligible wall-clock to a normal release (upload time is dominated by
    /// transfer, not start-spacing). Set to `"0s"` to disable pacing entirely
    /// (rely on the concurrency cap + reactive backoff). Override at runtime
    /// with `ANODIZER_GITHUB_UPLOAD_PACE_MS` (integer milliseconds; `0`
    /// disables).
    pub upload_pace: Option<HumanDuration>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `true` — a failure here aborts the release.
    /// Set to `false` to log failures but continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Explicit publish target — the SCM provider whose `release.<provider>`
    /// block the publisher uses. When set, overrides the implicit
    /// token-type fallback chain in
    /// [`crate::scm::resolve_token_type`].
    ///
    /// Use this for **cross-platform publishing**
    /// pattern: source repo on one provider (e.g. GitLab) but releases
    /// land on another (e.g. GitHub). Without it, the publish target
    /// is inferred from which `*_TOKEN` env-var is set — fine for
    /// single-provider setups but ambiguous when both tokens are
    /// available.
    ///
    /// ```yaml
    /// release:
    ///   provider: github
    ///   github:
    ///     owner: my-org
    ///     name: my-app
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ForceTokenKind>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
    /// In-process failure policy: what `anodizer release` does after a
    /// release-pipeline failure. `rollback` (default) deletes the run's
    /// release tag(s) and reverts the version-bump commit so the same
    /// version can be re-cut; `hold` leaves everything in place for
    /// forensics and manual recovery (`release --rollback-only
    /// --from-run=<id>`). `rollback` automatically degrades to `hold`
    /// the moment any one-way-door (Submitter) publisher has landed:
    /// the version is burned at a registry that never accepts it twice,
    /// so destructive rollback is refused and fix-forward is the only
    /// path. Root-level policy — in workspace configs (lockstep or
    /// per-crate) the top-level `release.on_failure` governs the whole
    /// run; setting it in a crate-level `release:` block is rejected at
    /// config load (`validate_on_failure_root_only`).
    pub on_failure: Option<OnFailureConfig>,
}

impl ReleaseConfig {
    /// Default release-name template (`"{{Tag}}"`).
    /// Anodize uses Tera-style `{{ Tag }}` (no dot prefix); the rendered
    /// value is identical for any tag the project produces.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ Tag }}";

    /// Default release `mode` (empty string is treated as
    /// "keep-existing" — keep current release notes, don't overwrite).
    pub const DEFAULT_MODE: &'static str = "keep-existing";

    /// Default minimum interval between successive asset-upload starts
    /// (see [`Self::upload_pace`]). 200 ms smooths the initial burst at the
    /// default concurrency of 4 without meaningfully slowing a release.
    pub const DEFAULT_UPLOAD_PACE: std::time::Duration = std::time::Duration::from_millis(200);

    /// Valid `mode:` values. Anything else is a config error.
    pub const VALID_MODES: &[&'static str] = &["keep-existing", "append", "prepend", "replace"];

    /// Resolve the `name_template`, falling back to
    /// [`Self::DEFAULT_NAME_TEMPLATE`].
    pub fn resolved_name_template(&self) -> &str {
        self.name_template
            .as_deref()
            .unwrap_or(Self::DEFAULT_NAME_TEMPLATE)
    }

    /// Resolve the release `mode`, validating and falling back to
    /// [`Self::DEFAULT_MODE`] when unset or empty. Returns an error when
    /// the user supplied a value outside [`Self::VALID_MODES`] so the
    /// invalid mode surfaces at the call site instead of producing a
    /// silent no-op publish.
    pub fn resolved_mode(&self) -> anyhow::Result<&str> {
        match self.mode.as_deref() {
            None | Some("") => Ok(Self::DEFAULT_MODE),
            Some(m) if Self::VALID_MODES.contains(&m) => Ok(m),
            Some(other) => Err(anyhow::anyhow!(
                "release: invalid mode '{}', must be one of: {}",
                other,
                Self::VALID_MODES.join(", ")
            )),
        }
    }

    /// Resolve `draft`, falling back to `false`.
    pub fn resolved_draft(&self) -> bool {
        self.draft.unwrap_or(false)
    }

    /// Resolve `replace_existing_draft`, falling back to `false`.
    pub fn resolved_replace_existing_draft(&self) -> bool {
        self.replace_existing_draft.unwrap_or(false)
    }

    /// Resolve `replace_existing_artifacts`, falling back to `false`.
    pub fn resolved_replace_existing_artifacts(&self) -> bool {
        self.replace_existing_artifacts.unwrap_or(false)
    }

    /// Resolve `include_meta`, falling back to `false` (don't upload
    /// metadata.json / artifacts.json as release assets by default).
    pub fn resolved_include_meta(&self) -> bool {
        self.include_meta.unwrap_or(false)
    }

    /// Resolve `use_existing_draft`, falling back to `false` (always
    /// create a fresh draft when one isn't found by default).
    pub fn resolved_use_existing_draft(&self) -> bool {
        self.use_existing_draft.unwrap_or(false)
    }

    /// Resolve `on_failure`, falling back to
    /// [`OnFailureConfig::Rollback`].
    pub fn resolved_on_failure(&self) -> OnFailureConfig {
        self.on_failure.unwrap_or_default()
    }

    /// Resolve the upload pace (minimum inter-upload-start interval) from the
    /// config, applying [`Self::DEFAULT_UPLOAD_PACE`] when unset. A configured
    /// `"0s"` resolves to `Duration::ZERO`, which the upload loop treats as
    /// "pacing disabled".
    ///
    /// Note: the runtime env override `ANODIZER_GITHUB_UPLOAD_PACE_MS` takes
    /// precedence and is applied at the call site (it needs the request-scoped
    /// [`crate::Context`]), mirroring how `ANODIZER_GITHUB_UPLOAD_CONCURRENCY`
    /// overrides [`Self::upload_concurrency`].
    pub fn resolved_upload_pace(&self) -> std::time::Duration {
        self.upload_pace
            .map(|d| d.duration())
            .unwrap_or(Self::DEFAULT_UPLOAD_PACE)
    }
}

/// In-process failure policy for `anodizer release`. See
/// [`ReleaseConfig::on_failure`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OnFailureConfig {
    /// Roll back reversible state (delete the run's release tags, revert
    /// the version-bump commit) so the version can be re-cut. Degrades to
    /// `Hold` when any one-way-door publisher already landed.
    #[default]
    Rollback,
    /// Leave everything in place for forensics; exit nonzero with a
    /// pointer at `release --rollback-only --from-run=<id>`.
    Hold,
}

impl std::fmt::Display for OnFailureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OnFailureConfig::Rollback => f.write_str("rollback"),
            OnFailureConfig::Hold => f.write_str("hold"),
        }
    }
}

/// Schema for prerelease: "auto" or boolean.
fn prerelease_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    auto_or_bool_schema()
}

/// Schema for make_latest: "auto" or boolean.
fn make_latest_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    auto_or_bool_schema()
}

/// Schema for skip_push: "auto" or boolean.
pub(super) fn skip_push_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    auto_or_bool_schema()
}

/// A `oneOf` schema accepting the literal string `"auto"` or any boolean — the
/// shape shared by `prerelease`, `make_latest`, and `skip_push` (each a
/// tri-state where `"auto"` defers the value to release-time inference).
fn auto_or_bool_schema() -> schemars::Schema {
    schemars::json_schema!({
        "oneOf": [
            { "type": "string", "enum": ["auto"] },
            { "type": "boolean" }
        ]
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScmRepoConfig {
    /// Repository owner (user or organization).
    pub owner: String,
    /// Repository name.
    pub name: String,
}

/// Backward-compatible alias — existing code can continue to use `GitHubConfig`.
pub type GitHubConfig = ScmRepoConfig;

// ---------------------------------------------------------------------------
// ForceTokenKind
// ---------------------------------------------------------------------------

/// Which SCM token to force for authentication, overriding automatic detection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ForceTokenKind {
    GitHub,
    GitLab,
    Gitea,
}

// ---------------------------------------------------------------------------
// Platform URL configs (GitHub Enterprise, GitLab self-hosted, Gitea)
// ---------------------------------------------------------------------------

/// Custom GitHub API/upload/download URLs for GitHub Enterprise installations.
/// GitHub API/download URL overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GitHubUrlsConfig {
    /// GitHub API base URL (e.g. `https://github.example.com/api/v3/`).
    pub api: Option<String>,
    /// GitHub upload URL for release assets (e.g. `https://github.example.com/api/uploads/`).
    pub upload: Option<String>,
    /// GitHub download URL for release assets (e.g. `https://github.example.com/`).
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
}

/// Custom GitLab API/download URLs for self-hosted GitLab installations.
/// GitLab API/download URL overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GitLabUrlsConfig {
    /// GitLab API base URL (e.g. `https://gitlab.example.com/api/v4/`).
    pub api: Option<String>,
    /// GitLab download URL for release assets.
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
    /// When true, use the GitLab Package Registry for uploads instead of Generic Packages.
    pub use_package_registry: Option<bool>,
    /// When true, use the CI_JOB_TOKEN for authentication instead of a personal token.
    pub use_job_token: Option<bool>,
}

/// Custom Gitea API/download URLs for self-hosted Gitea installations.
/// Gitea API/download URL overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GiteaUrlsConfig {
    /// Gitea API base URL (e.g. `https://gitea.example.com/api/v1/`).
    pub api: Option<String>,
    /// Gitea download URL for release assets.
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
}

// ---------------------------------------------------------------------------
// "auto" | bool enum — shared serde implementation
// ---------------------------------------------------------------------------

/// Generates `Serialize` and `Deserialize` impls for enums with `Auto` and
/// `Bool(bool)` variants that accept the string `"auto"` or a boolean in YAML.
macro_rules! impl_auto_or_bool_serde {
    ($ty:ty, $auto:path, $bool_variant:path) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                match self {
                    $auto => serializer.serialize_str("auto"),
                    $bool_variant(b) => serializer.serialize_bool(*b),
                }
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                struct Visitor;
                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "\"auto\" or a boolean")
                    }
                    fn visit_bool<E: serde::de::Error>(
                        self,
                        v: bool,
                    ) -> std::result::Result<$ty, E> {
                        Ok($bool_variant(v))
                    }
                    fn visit_str<E: serde::de::Error>(
                        self,
                        v: &str,
                    ) -> std::result::Result<$ty, E> {
                        if v == "auto" {
                            Ok($auto)
                        } else {
                            Err(E::custom(format!("expected \"auto\", got \"{}\"", v)))
                        }
                    }
                }
                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

/// `prerelease` can be the string `"auto"` or a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereleaseConfig {
    Auto,
    Bool(bool),
}

impl_auto_or_bool_serde!(
    PrereleaseConfig,
    PrereleaseConfig::Auto,
    PrereleaseConfig::Bool
);

/// `make_latest` can be the string `"auto"`, a boolean, or a template string.
/// This field is rendered through the template engine at publish time,
/// so we accept arbitrary strings (e.g. `"{{ if .IsSnapshot }}false{{ else }}true{{ end }}"`)
/// and defer resolution to the release stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakeLatestConfig {
    Auto,
    Bool(bool),
    /// An arbitrary template string to be rendered at publish time.
    String(String),
}

impl Serialize for MakeLatestConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            MakeLatestConfig::Auto => serializer.serialize_str("auto"),
            MakeLatestConfig::Bool(b) => serializer.serialize_bool(*b),
            MakeLatestConfig::String(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for MakeLatestConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = MakeLatestConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\", a boolean, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<MakeLatestConfig, E> {
                Ok(MakeLatestConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<MakeLatestConfig, E> {
                match v {
                    "auto" => Ok(MakeLatestConfig::Auto),
                    "true" => Ok(MakeLatestConfig::Bool(true)),
                    "false" => Ok(MakeLatestConfig::Bool(false)),
                    other => Ok(MakeLatestConfig::String(other.to_string())),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// `skip_push` can be `"auto"` (skip for prereleases), a boolean, or a template string.
/// Template expressions like `"{{ if .IsSnapshot }}true{{ end }}"` are accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipPushConfig {
    Auto,
    Bool(bool),
    /// Arbitrary template string — rendered at runtime, truthy result means skip push.
    Template(String),
}

impl Serialize for SkipPushConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            SkipPushConfig::Auto => serializer.serialize_str("auto"),
            SkipPushConfig::Bool(b) => serializer.serialize_bool(*b),
            SkipPushConfig::Template(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for SkipPushConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = SkipPushConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "\"auto\", a boolean, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<SkipPushConfig, E> {
                Ok(SkipPushConfig::Bool(v))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<SkipPushConfig, E> {
                match v {
                    "auto" => Ok(SkipPushConfig::Auto),
                    "true" => Ok(SkipPushConfig::Bool(true)),
                    "false" => Ok(SkipPushConfig::Bool(false)),
                    other => Ok(SkipPushConfig::Template(other.to_string())),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The three `"auto" | bool | [template]` config enums share a hand-written
    // serde `Visitor` per type; each arm (string-auto, bool, template fallback,
    // and the bad-string error) is exercised here through a YAML round-trip.

    #[test]
    fn prerelease_config_parses_auto_and_bools() {
        assert_eq!(
            serde_yaml_ng::from_str::<PrereleaseConfig>("auto").unwrap(),
            PrereleaseConfig::Auto
        );
        assert_eq!(
            serde_yaml_ng::from_str::<PrereleaseConfig>("true").unwrap(),
            PrereleaseConfig::Bool(true)
        );
        assert_eq!(
            serde_yaml_ng::from_str::<PrereleaseConfig>("false").unwrap(),
            PrereleaseConfig::Bool(false)
        );
    }

    #[test]
    fn prerelease_config_rejects_other_strings() {
        // Unlike the templated siblings, `prerelease` only accepts "auto".
        assert!(serde_yaml_ng::from_str::<PrereleaseConfig>("maybe").is_err());
    }

    #[test]
    fn prerelease_config_round_trips_through_serialize() {
        for v in [
            PrereleaseConfig::Auto,
            PrereleaseConfig::Bool(true),
            PrereleaseConfig::Bool(false),
        ] {
            let yaml = serde_yaml_ng::to_string(&v).unwrap();
            assert_eq!(
                serde_yaml_ng::from_str::<PrereleaseConfig>(&yaml).unwrap(),
                v
            );
        }
    }

    #[test]
    fn make_latest_config_parses_all_arms() {
        assert_eq!(
            serde_yaml_ng::from_str::<MakeLatestConfig>("auto").unwrap(),
            MakeLatestConfig::Auto
        );
        assert_eq!(
            serde_yaml_ng::from_str::<MakeLatestConfig>("true").unwrap(),
            MakeLatestConfig::Bool(true)
        );
        assert_eq!(
            serde_yaml_ng::from_str::<MakeLatestConfig>("false").unwrap(),
            MakeLatestConfig::Bool(false)
        );
        // A non-keyword string falls through to a deferred template.
        assert_eq!(
            serde_yaml_ng::from_str::<MakeLatestConfig>(
                "\"{{ if .IsSnapshot }}false{{ else }}true{{ end }}\""
            )
            .unwrap(),
            MakeLatestConfig::String(
                "{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string()
            )
        );
    }

    #[test]
    fn make_latest_config_round_trips_through_serialize() {
        for v in [
            MakeLatestConfig::Auto,
            MakeLatestConfig::Bool(false),
            MakeLatestConfig::String("{{ .Env.LATEST }}".to_string()),
        ] {
            let yaml = serde_yaml_ng::to_string(&v).unwrap();
            assert_eq!(
                serde_yaml_ng::from_str::<MakeLatestConfig>(&yaml).unwrap(),
                v
            );
        }
    }

    #[test]
    fn skip_push_config_parses_all_arms() {
        assert_eq!(
            serde_yaml_ng::from_str::<SkipPushConfig>("auto").unwrap(),
            SkipPushConfig::Auto
        );
        assert_eq!(
            serde_yaml_ng::from_str::<SkipPushConfig>("true").unwrap(),
            SkipPushConfig::Bool(true)
        );
        assert_eq!(
            serde_yaml_ng::from_str::<SkipPushConfig>("false").unwrap(),
            SkipPushConfig::Bool(false)
        );
        assert_eq!(
            serde_yaml_ng::from_str::<SkipPushConfig>("\"{{ .IsSnapshot }}\"").unwrap(),
            SkipPushConfig::Template("{{ .IsSnapshot }}".to_string())
        );
    }

    #[test]
    fn skip_push_config_round_trips_through_serialize() {
        for v in [
            SkipPushConfig::Auto,
            SkipPushConfig::Bool(true),
            SkipPushConfig::Template("{{ .IsSnapshot }}".to_string()),
        ] {
            let yaml = serde_yaml_ng::to_string(&v).unwrap();
            assert_eq!(serde_yaml_ng::from_str::<SkipPushConfig>(&yaml).unwrap(), v);
        }
    }
}
