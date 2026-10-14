use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ContentSource, ExtraFileSpec, HumanDuration, StringOrBool, TemplatedExtraFile,
    deserialize_string_or_bool_opt,
};

// ReleaseConfig

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReleaseConfig {
    /// GitHub repository to release to (owner and name, inferred from the
    /// `origin` remote when omitted).
    pub github: Option<ScmRepoConfig>,
    /// GitLab repository to release to (owner and name, inferred from the
    /// `origin` remote when omitted).
    pub gitlab: Option<ScmRepoConfig>,
    /// Gitea repository to release to (owner and name, inferred from the
    /// `origin` remote when omitted).
    pub gitea: Option<ScmRepoConfig>,
    /// When true, create the release as a draft (unpublished).
    pub draft: Option<bool>,
    #[schemars(schema_with = "prerelease_schema")]
    /// Mark release as pre-release: `true`, `false`, or `"auto"` (inferred
    /// from the tag). The words `true`, `false`, `1`, `0` and `auto` load in
    /// any letter case, quoted or unquoted; any other string is a load
    /// error.
    ///
    /// ```yaml
    /// release:
    ///   prerelease: auto
    /// ```
    pub prerelease: Option<PrereleaseConfig>,
    #[schemars(schema_with = "make_latest_schema")]
    /// Mark release as latest: `true`, `false`, `"auto"` (GitHub decides:
    /// the release is sent with `make_latest: legacy`), or a template
    /// rendered at publish time. The words `true`, `false`, `1`, `0` and
    /// `auto` load in any letter case, quoted or unquoted; any other string
    /// is a template. A template that renders `false`, `0` or
    /// empty does not mark the release latest, `auto` lets GitHub decide, and
    /// `true`, `1` or any other render marks it latest.
    ///
    /// ```yaml
    /// release:
    ///   make_latest: "{{ not IsSnapshot }}"
    /// ```
    pub make_latest: Option<MakeLatestConfig>,
    /// Release title template (supports templates).
    pub name_template: Option<String>,
    /// Text prepended to the release body (inline string, from_file, or from_url).
    pub header: Option<ContentSource>,
    /// Text appended to the release body (inline string, from_file, or from_url).
    ///
    /// REPLACES anodizer's default attribution footer; the two are never combined. Set to `""` for no footer at all.
    pub footer: Option<ContentSource>,
    /// Append a derived `**Full Changelog**: <compare-url>` line to the
    /// release body, below the changelog and above the footer. The compare
    /// range is `<previous tag>...<tag>` on the repository this release
    /// publishes to. Suppressed automatically when no previous tag exists
    /// (a first release) or when the changelog body already carries such a
    /// line (`changelog.use: github-native`). Default: `true`.
    ///
    /// ```yaml
    /// release:
    ///   full_changelog_link: false   # do not append the derived compare link
    /// ```
    pub full_changelog_link: Option<bool>,
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
    /// When true, replace an existing draft release. When false, an existing draft fails the release.
    pub replace_existing_draft: Option<bool>,
    /// When true, replace existing release artifacts with the same name.
    ///
    /// A nightly run with `nightly.tag_name` set behaves as if this were
    /// `true`: the fixed tag is rolling, so every run after the first meets
    /// the assets its predecessor uploaded onto the same release.
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
    /// Glob patterns matched against each release asset's file name; anodizer
    /// drops any asset whose name matches at least one glob before attaching it
    /// to THIS GitHub release only (a mirror configured elsewhere is
    /// unaffected). Use it to keep heavy sidecars (checksums, signatures,
    /// SBOMs) off the GitHub release while archives still attach. Composes with
    /// `ids:` (both filters apply). Unset or empty keeps everything.
    ///
    /// ```yaml
    /// release:
    ///   github: { owner: my-org, name: my-repo }
    ///   exclude: ["*.sha256", "*.sig", "*.cdx.json"]
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// Target branch or SHA for the release tag.
    pub target_commitish: Option<String>,
    /// GitHub Discussion category name for the release.
    pub discussion_category_name: Option<String>,
    /// Upload `dist/metadata.json` as a release asset. The sibling
    /// `dist/artifacts.json` manifest is never uploaded — it stays local to
    /// the dist directory.
    pub include_meta: Option<bool>,
    /// Reuse an existing draft release whose name matches the rendered
    /// `name_template` in place of creating a new one.
    pub use_existing_draft: Option<bool>,
    /// Override the release tag (template string). When set, this tag is used
    /// as the `tag_name` in the GitHub release API in place of the crate's
    /// `tag_template`. Useful in monorepo setups to strip a tag prefix
    /// (e.g. `"{{ Tag }}"` to publish `v1.0.0` for the tag `myapp/v1.0.0`).
    /// A cross-platform publishing feature provided for free by anodizer.
    pub tag: Option<String>,
    /// Maximum number of asset-upload requests in flight simultaneously.
    /// Applies to asset uploads on every release forge (GitHub, GitLab,
    /// Gitea).
    ///
    /// GitHub's secondary rate-limit is triggered by burst traffic. Keeping
    /// this value low avoids tripping the limit even for releases with many
    /// artifacts. Default: 4. Override at runtime with
    /// `ANODIZER_GITHUB_UPLOAD_CONCURRENCY`.
    pub upload_concurrency: Option<u32>,
    /// Minimum interval between successive asset-upload *starts* (a humantime
    /// string, e.g. `"200ms"`, `"1s"`, `"0s"`). Applies to asset uploads on
    /// every release forge (GitHub, GitLab, Gitea).
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
    /// transfer; start-spacing adds little). Set to `"0s"` to disable pacing entirely
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
    /// Explicit publish target: the SCM provider (`github`, `gitlab` or
    /// `gitea`) whose `release.<provider>` block the publisher uses. The
    /// provider is the first of these that gives an answer:
    ///
    /// 1. `release.provider`;
    /// 2. the top-level `force_token`;
    /// 3. the `ANODIZER_FORCE_TOKEN` environment variable; when it is unset
    ///    or blank, `GORELEASER_FORCE_TOKEN` is read; both are trimmed
    ///    (`github`, `gitlab` or `gitea` in any letter case; any other value
    ///    gives no answer);
    /// 4. `GITLAB_TOKEN` set in the environment, or a GitLab token file:
    ///    GitLab;
    /// 5. `GITEA_TOKEN` set in the environment, or a Gitea token file: Gitea;
    /// 6. when no forge variable or token file holds a token, the release
    ///    blocks: every one naming `gitlab:` only gives GitLab, every one
    ///    naming `gitea:` only gives Gitea;
    /// 7. GitHub.
    ///
    /// A token line in a listed env file (the list form of `env_files`)
    /// never picks the provider. Once the provider is chosen, its line is
    /// the last source of the run token, after `--token`, the provider's
    /// variables and its token file.
    ///
    /// Set it for cross-platform publishing, where the source repo lives on
    /// one provider (e.g. GitLab) and releases go to another (e.g. GitHub),
    /// or whenever more than one forge token is in the environment.
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
    /// place and does not try to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
    /// In-process failure policy: what `anodizer release` does after a
    /// release-pipeline failure. `hold` is the only accepted value, and it
    /// describes what the pipeline now does unconditionally — leave
    /// everything in place for forensics. Recovery is a re-run (publishers
    /// reconcile and self-skip, so an identical command converges on
    /// already-published state) or, for deliberate withdrawal,
    /// `anodizer tag rollback`. `rollback` is rejected when the config
    /// loads, because automatic rollback was removed; the field exists so a
    /// config carrying the removed policy is refused with an error and
    /// keeps no other meaning. Root-level policy: in workspace configs
    /// (lockstep or per-crate) the top-level `release.on_failure` governs
    /// the whole run, and setting it in a crate-level `release:` block is
    /// rejected when the config loads.
    pub on_failure: Option<OnFailureConfig>,
    /// How the release preflight treats a failed check. The release preflight
    /// runs before anything is built or published and asks the forge the
    /// release is created on whether this run can create it.
    ///
    /// ```yaml
    /// release:
    ///   preflight:
    ///     fail_on_error: false   # report a failed check as a warning and keep going
    /// ```
    pub preflight: Option<ReleasePreflightConfig>,
}

/// The `release.preflight:` block.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ReleasePreflightConfig {
    /// Stop the run when a release preflight check fails. Default: `true`.
    ///
    /// The checks, per forge:
    ///
    /// - GitHub: the token can push to the release repository, and when a
    ///   release already exists at the tag it is not immutable. An immutable
    ///   release cannot be updated and its assets cannot be replaced, so the
    ///   release would fail.
    /// - GitLab: the token has Developer access or higher on the project.
    ///   A run authenticating with a CI job token skips this check, since a
    ///   job token cannot read the project.
    /// - Gitea: the token can push to the repository.
    /// - A missing token is a failed check on every forge.
    ///
    /// With `false`, a failed check is reported as a warning that ends in
    /// `continuing because release.preflight.fail_on_error is false`, and
    /// the run continues; the release then fails at creation if the problem
    /// is real. The token is also an environment requirement of the release
    /// (`ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` or `GH_TOKEN`; `GITLAB_TOKEN`;
    /// `GITEA_TOKEN`; or `--token`), so a missing token still stops the run at
    /// the environment check whatever this field says, and with `false` the
    /// release preflight's own report of it is a warning beside that failure.
    /// A check that `preflight.strict` turns into a failure is a failed check
    /// here too.
    ///
    /// A crate's own value wins over the top-level `release:` block's; an
    /// empty value counts as unset. Accepts a bool, an unquoted `1` or `0`,
    /// or a template that renders to `true`, `false`, `1` or `0` (any letter
    /// case). Any other render, an empty one included, stops the run with an
    /// error naming the value.
    ///
    /// ```yaml
    /// release:
    ///   preflight:
    ///     fail_on_error: "{{ not IsNightly }}"   # warn only on nightly runs
    /// ```
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    #[schemars(extend("default" = true))]
    pub fail_on_error: Option<StringOrBool>,
}

impl ReleaseConfig {
    /// Default release-name template (`"{{Tag}}"`).
    /// Anodizer uses Tera-style `{{ Tag }}` (no dot prefix); the rendered
    /// value is identical for any tag the project produces.
    pub const DEFAULT_NAME_TEMPLATE: &'static str = "{{ Tag }}";

    /// Default release `mode` (empty string is treated as
    /// "keep-existing" — keep current release notes, don't overwrite).
    pub const DEFAULT_MODE: &'static str = "keep-existing";

    /// Default release-body footer: the anodizer attribution line. Used when
    /// neither `release.footer` nor `changelog.footer` is set. An explicit
    /// `release.footer: ""` suppresses it.
    pub const DEFAULT_FOOTER: &'static str =
        "Released with [anodizer](https://github.com/tj-smith47/anodizer) 🦀";

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
    /// invalid mode fails at the call site and no silent no-op publish
    /// happens.
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

    /// Resolve `full_changelog_link`, falling back to `true` — the derived
    /// compare link is on unless turned off.
    pub fn resolved_full_changelog_link(&self) -> bool {
        self.full_changelog_link.unwrap_or(true)
    }

    /// Resolve `include_meta`, falling back to `false` (don't upload
    /// metadata.json as a release asset by default).
    pub fn resolved_include_meta(&self) -> bool {
        self.include_meta.unwrap_or(false)
    }

    /// Resolve `use_existing_draft`, falling back to `false` (always
    /// create a fresh draft when one isn't found by default).
    pub fn resolved_use_existing_draft(&self) -> bool {
        self.use_existing_draft.unwrap_or(false)
    }

    /// Resolve the upload pace (minimum inter-upload-start interval) from the
    /// config, applying [`Self::DEFAULT_UPLOAD_PACE`] when unset. A configured
    /// `"0s"` resolves to `Duration::ZERO`, which the upload loop treats as
    /// "pacing disabled".
    ///
    /// Note: the runtime env override `ANODIZER_GITHUB_UPLOAD_PACE_MS` takes
    /// precedence and is applied at the call site (it needs the request-scoped
    /// [`crate::context::Context`]), mirroring how `ANODIZER_GITHUB_UPLOAD_CONCURRENCY`
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
    /// Removed: automatic rollback (delete the run's release tags, revert
    /// the version-bump commit). Rejected at config load
    /// (`validate_on_failure_not_rollback`) — re-running `anodizer release`
    /// converges on already-published state, and `anodizer tag rollback`
    /// handles deliberate withdrawal. Kept as a parseable variant so the
    /// validator can name it in the migration error; an opaque
    /// deserialize failure would not.
    Rollback,
    /// Leave everything in place for forensics; exit nonzero with a
    /// pointer at `anodizer tag rollback`.
    #[default]
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

/// Schema for prerelease: `"auto"`, a boolean, an unquoted `1` / `0`, or one
/// of the strings `auto`, `true`, `false`, `1` and `0`, trimmed and in any
/// letter case, as the loader reads them.
fn prerelease_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "oneOf": [
            { "type": "boolean" },
            super::string_or_bool::one_or_zero_schema(),
            {
                "type": "string",
                "pattern": r"^\s*([Aa][Uu][Tt][Oo]|[Tt][Rr][Uu][Ee]|[Ff][Aa][Ll][Ss][Ee]|1|0)\s*$"
            }
        ]
    })
}

/// Schema for make_latest: `"auto"`, a boolean, an unquoted `1` / `0`, or a
/// template string.
pub(super) fn make_latest_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    auto_bool_or_template_schema()
}

/// Schema for skip_push: `"auto"`, a boolean, an unquoted `1` / `0`, or a
/// template string.
pub(super) fn skip_push_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    auto_bool_or_template_schema()
}

/// The shape `make_latest` and `skip_push` share: `"auto"` defers the value to
/// release time, and any other string is a template rendered then.
fn auto_bool_or_template_schema() -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            { "type": "boolean" },
            super::string_or_bool::one_or_zero_schema(),
            { "type": "string" }
        ]
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScmRepoConfig {
    /// Repository owner (user or organization). Omitted or empty: inferred
    /// from the `origin` git remote, so a block may name only `token`.
    #[serde(default)]
    pub owner: String,
    /// Repository name. Omitted or empty: inferred from the `origin` git
    /// remote together with `owner`.
    #[serde(default)]
    pub name: String,
    /// Auth token override for this repository, used in place of the default one.
    /// Useful when the release repository requires a different token than
    /// the one used to build from — for instance, when publishing to a
    /// repository in another organization. Supports templates
    /// (environment variables). Unset falls back to the pipeline token.
    ///
    /// ```yaml
    /// release:
    ///   github:
    ///     owner: my-org
    ///     name: my-repo
    ///     token: "{{ .Env.RELEASE_GITHUB_TOKEN }}"
    /// ```
    ///
    /// The token alone is enough when the release repository is the one
    /// `origin` points at:
    ///
    /// ```yaml
    /// release:
    ///   github:
    ///     token: "{{ .Env.RELEASE_GITHUB_TOKEN }}"
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// Backward-compatible alias — existing code can continue to use `GitHubConfig`.
pub type GitHubConfig = ScmRepoConfig;

// ForceTokenKind

/// Which SCM token to force for authentication, overriding automatic detection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ForceTokenKind {
    GitHub,
    GitLab,
    Gitea,
}

// Platform URL configs (GitHub Enterprise, GitLab self-hosted, Gitea)

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
    /// When true, use the GitLab Package Registry for uploads. When false, uploads go to Generic Packages.
    pub use_package_registry: Option<bool>,
    /// When true, authenticate with the CI_JOB_TOKEN. When false, a personal token is used.
    pub use_job_token: Option<bool>,
}

/// Custom Gitea API/download URLs for self-hosted Gitea installations.
/// Gitea API/download URL overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GiteaUrlsConfig {
    /// Gitea instance URL (e.g. `https://gitea.example.com`).
    ///
    /// The release backend appends `/api/v1/…` itself; a value that already
    /// ends in `/api/v1` is accepted and trimmed back to the instance root.
    pub api: Option<String>,
    /// Gitea download URL for release assets.
    pub download: Option<String>,
    /// When true, skip TLS certificate verification for the custom URLs.
    pub skip_tls_verify: Option<bool>,
}

// "auto" | bool enum — shared serde implementation

/// Generates `Serialize` and `Deserialize` impls for enums with `Auto` and
/// `Bool(bool)` variants that accept the string `"auto"`, a boolean, an
/// unquoted `1` / `0`, or one of the strings `auto`, `true`, `false`, `1`
/// and `0` (trimmed, any letter case) in YAML.
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
                        write!(f, "\"auto\", a boolean, 1 or 0")
                    }
                    fn visit_bool<E: serde::de::Error>(
                        self,
                        v: bool,
                    ) -> std::result::Result<$ty, E> {
                        Ok($bool_variant(v))
                    }
                    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<$ty, E> {
                        super::string_or_bool::bool_from_u64(v, &self).map($bool_variant)
                    }
                    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<$ty, E> {
                        super::string_or_bool::bool_from_i64(v, &self).map($bool_variant)
                    }
                    fn visit_str<E: serde::de::Error>(
                        self,
                        v: &str,
                    ) -> std::result::Result<$ty, E> {
                        super::string_or_bool::auto_or_bool_word(v, $auto, $bool_variant)
                            .ok_or_else(|| {
                                E::custom(format!(
                                    "expected \"auto\", true, false, 1 or 0, got \"{}\"",
                                    v
                                ))
                            })
                    }
                }
                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

/// `prerelease` can be the string `"auto"` or a boolean. The words `true`,
/// `false`, `1`, `0` and `auto` load in any letter case, quoted or
/// unquoted; a quoted word is trimmed first.
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
/// The words `true`, `false`, `1`, `0` and `auto` load in any letter case,
/// quoted or unquoted; a quoted word is trimmed first.
/// This field is rendered through the template engine at publish time,
/// so arbitrary strings are accepted (e.g. `"{{ if .IsSnapshot }}false{{ else }}true{{ end }}"`)
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
                write!(f, "\"auto\", a boolean, 1, 0, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<MakeLatestConfig, E> {
                Ok(MakeLatestConfig::Bool(v))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                v: u64,
            ) -> std::result::Result<MakeLatestConfig, E> {
                super::string_or_bool::bool_from_u64(v, &self).map(MakeLatestConfig::Bool)
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                v: i64,
            ) -> std::result::Result<MakeLatestConfig, E> {
                super::string_or_bool::bool_from_i64(v, &self).map(MakeLatestConfig::Bool)
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<MakeLatestConfig, E> {
                Ok(super::string_or_bool::auto_or_bool_word(
                    v,
                    MakeLatestConfig::Auto,
                    MakeLatestConfig::Bool,
                )
                .unwrap_or_else(|| MakeLatestConfig::String(v.to_string())))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// `skip_push` can be `"auto"` (skip for prereleases), a boolean, or a template string.
/// The words `true`, `false`, `1`, `0` and `auto` load in any letter case,
/// quoted or unquoted; a quoted word is trimmed first.
/// Template expressions like `"{{ if .IsSnapshot }}true{{ end }}"` are accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipPushConfig {
    Auto,
    Bool(bool),
    /// Template string rendered at runtime and read trimmed and in any letter
    /// case: `true` or `1` skips the push, `auto` skips it on a prerelease,
    /// anything else pushes.
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
                write!(f, "\"auto\", a boolean, 1, 0, or a template string")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<SkipPushConfig, E> {
                Ok(SkipPushConfig::Bool(v))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                v: u64,
            ) -> std::result::Result<SkipPushConfig, E> {
                super::string_or_bool::bool_from_u64(v, &self).map(SkipPushConfig::Bool)
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                v: i64,
            ) -> std::result::Result<SkipPushConfig, E> {
                super::string_or_bool::bool_from_i64(v, &self).map(SkipPushConfig::Bool)
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<SkipPushConfig, E> {
                Ok(super::string_or_bool::auto_or_bool_word(
                    v,
                    SkipPushConfig::Auto,
                    SkipPushConfig::Bool,
                )
                .unwrap_or_else(|| SkipPushConfig::Template(v.to_string())))
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
        // `prerelease` takes no template, so a string must spell a value.
        assert!(serde_yaml_ng::from_str::<PrereleaseConfig>("maybe").is_err());
    }

    /// Every spelling of a bool each type takes, and the numbers it refuses.
    fn assert_one_and_zero<T>(t: fn(bool) -> T)
    where
        T: for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        for (yaml, b) in [
            ("1", true),
            ("0", false),
            ("\"1\"", true),
            ("\"0\"", false),
            ("true", true),
            ("\"false\"", false),
        ] {
            assert_eq!(serde_yaml_ng::from_str::<T>(yaml).unwrap(), t(b), "{yaml}");
        }
        for bad in ["2", "-1"] {
            let err = serde_yaml_ng::from_str::<T>(bad).unwrap_err().to_string();
            assert!(err.contains("invalid value"), "{bad}: {err}");
        }
    }

    #[test]
    fn prerelease_config_reads_one_and_zero_as_bools() {
        assert_one_and_zero(PrereleaseConfig::Bool);
    }

    #[test]
    fn make_latest_config_reads_one_and_zero_as_bools() {
        assert_one_and_zero(MakeLatestConfig::Bool);
    }

    #[test]
    fn skip_push_config_reads_one_and_zero_as_bools() {
        assert_one_and_zero(SkipPushConfig::Bool);
    }

    /// A quoted `TRUE`, ` False ` or `AUTO` loads the way the same string
    /// reads after a render: trimmed and in any letter case. The schema
    /// accepts what the loader accepts on `prerelease`.
    #[test]
    fn a_quoted_bool_word_loads_in_any_letter_case() {
        let cases = [
            ("\"TRUE\"", Some(true)),
            ("\" False \"", Some(false)),
            ("\"1\"", Some(true)),
            ("\"0\"", Some(false)),
            ("\"AUTO\"", None),
        ];
        for (yaml, want) in cases {
            let prerelease: PrereleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
            let make_latest: MakeLatestConfig = serde_yaml_ng::from_str(yaml).unwrap();
            let skip_push: SkipPushConfig = serde_yaml_ng::from_str(yaml).unwrap();
            match want {
                Some(b) => {
                    assert_eq!(prerelease, PrereleaseConfig::Bool(b), "{yaml}");
                    assert_eq!(make_latest, MakeLatestConfig::Bool(b), "{yaml}");
                    assert_eq!(skip_push, SkipPushConfig::Bool(b), "{yaml}");
                }
                None => {
                    assert_eq!(prerelease, PrereleaseConfig::Auto, "{yaml}");
                    assert_eq!(make_latest, MakeLatestConfig::Auto, "{yaml}");
                    assert_eq!(skip_push, SkipPushConfig::Auto, "{yaml}");
                }
            }
        }
        assert!(serde_yaml_ng::from_str::<PrereleaseConfig>("\"yes\"").is_err());
        assert_eq!(
            serde_yaml_ng::from_str::<MakeLatestConfig>("\"{{ .IsSnapshot }}\"").unwrap(),
            MakeLatestConfig::String("{{ .IsSnapshot }}".into())
        );

        let validator = jsonschema::validator_for(&crate::config::config_schema()).unwrap();
        for (v, ok) in [
            ("TRUE", true),
            (" False ", true),
            ("AUTO", true),
            ("yes", false),
        ] {
            let instance = serde_json::json!({ "release": { "prerelease": v } });
            assert_eq!(validator.is_valid(&instance), ok, "schema on {v:?}");
        }
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

    #[test]
    fn resolved_upload_pace_defaults_and_honors_override() {
        // Unset → the 200ms default that smooths the initial upload burst.
        assert_eq!(
            ReleaseConfig::default().resolved_upload_pace(),
            ReleaseConfig::DEFAULT_UPLOAD_PACE
        );
        // A configured humantime value is used verbatim.
        let cfg = ReleaseConfig {
            upload_pace: Some(HumanDuration(std::time::Duration::from_secs(1))),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_upload_pace(),
            std::time::Duration::from_secs(1)
        );
        // "0s" disables pacing (resolves to ZERO, not the default).
        let disabled = ReleaseConfig {
            upload_pace: Some(HumanDuration(std::time::Duration::ZERO)),
            ..Default::default()
        };
        assert_eq!(disabled.resolved_upload_pace(), std::time::Duration::ZERO);
    }

    #[test]
    fn on_failure_config_display_and_default() {
        // `Hold` is the only surviving behavior; `Rollback` still parses
        // (so config validation can name it in the migration error) but
        // is no longer the default and is rejected at load time.
        assert_eq!(OnFailureConfig::default(), OnFailureConfig::Hold);
        assert_eq!(OnFailureConfig::Rollback.to_string(), "rollback");
        assert_eq!(OnFailureConfig::Hold.to_string(), "hold");
        // The lowercase serde form round-trips.
        assert_eq!(
            serde_yaml_ng::from_str::<OnFailureConfig>("hold").unwrap(),
            OnFailureConfig::Hold
        );
        assert_eq!(
            serde_yaml_ng::from_str::<OnFailureConfig>("rollback").unwrap(),
            OnFailureConfig::Rollback
        );
    }
}
