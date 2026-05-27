use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Include specification types
// ---------------------------------------------------------------------------

/// An include specification: either a plain path string or a structured from_file/from_url.
///
/// YAML examples:
/// ```yaml
/// includes:
///   - ./defaults.yaml                           # plain string (backward compat)
///   - from_file:
///       path: ./config/goreleaser.yaml           # structured file path
///   - from_url:
///       url: https://example.com/config.yaml     # URL fetch
///       headers:
///         x-api-token: "${MYCOMPANY_TOKEN}"       # env var expansion in headers
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum IncludeSpec {
    /// Plain string path (backward compatible): "path/to/file.yaml"
    Path(String),
    /// Structured file include with `from_file.path`.
    FromFile { from_file: IncludeFilePath },
    /// Structured URL include with `from_url.url` and optional headers.
    FromUrl { from_url: IncludeUrlConfig },
}

/// File path for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct IncludeFilePath {
    /// Path to the include file (relative to the config file).
    pub path: String,
}

/// URL configuration for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct IncludeUrlConfig {
    /// URL to fetch. If it does not start with `http://` or `https://`,
    /// `https://raw.githubusercontent.com/` is prepended (GitHub shorthand).
    pub url: String,
    /// Optional HTTP headers. Values support `${VAR_NAME}` environment variable expansion.
    pub headers: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` rejects typos and unknown config
/// fields at parse time, matching GoReleaser's `yaml.UnmarshalStrict`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Schema version. Currently supports 1 (implicit default) and 2.
    pub version: Option<u32>,
    /// Human-readable project name used in templates and release titles.
    pub project_name: String,
    /// Output directory for build artifacts (default: ./dist).
    #[serde(default = "default_dist")]
    pub dist: PathBuf,
    /// Additional config files to merge into this config.
    /// Supports plain string paths, `from_file:` for structured file paths,
    /// and `from_url:` for fetching configs from URLs with optional headers.
    pub includes: Option<Vec<IncludeSpec>>,
    /// Environment file configuration. Accepts either:
    /// - A list of `.env` file paths: `[".env", ".release.env"]`
    /// - A struct with token file paths: `{ github_token: "~/.config/goreleaser/github_token" }`
    pub env_files: Option<EnvFilesConfig>,
    /// Default values applied to all crates unless overridden.
    pub defaults: Option<Defaults>,
    /// Hooks run before the release pipeline starts.
    pub before: Option<HooksConfig>,
    /// Hooks run after the release pipeline completes.
    pub after: Option<HooksConfig>,
    /// Hooks run after build/archive/sign/sbom/checksum complete but
    /// immediately before the publish phase dispatches any publisher.
    ///
    /// Use cases: smoke-test artifacts against the staged dist tree,
    /// run external validators (antivirus, vulnerability scanners),
    /// stage external state, or abort the release before any
    /// publisher writes to a registry.
    ///
    /// A non-zero exit code from any hook aborts the release before
    /// publish runs. Hooks fire in declared order. Use `--skip=before-publish`
    /// to bypass.
    pub before_publish: Option<HooksConfig>,
    /// List of crates in this project.
    pub crates: Vec<CrateConfig>,
    /// Changelog generation configuration.
    pub changelog: Option<ChangelogConfig>,
    /// Signing configurations for binaries, archives, and checksums.
    #[serde(default, deserialize_with = "deserialize_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub signs: Vec<SignConfig>,
    /// Binary-specific signing configs (same shape as `signs` but only for
    /// binary artifacts). The `artifacts` field on each entry is constrained
    /// at parse time to `binary` / `none` (or omitted) — a broader filter on
    /// `binary_signs` would silently match nothing because the loop only
    /// iterates Binary artifacts. Constraint lives in `deserialize_binary_signs`.
    #[serde(default, deserialize_with = "deserialize_binary_signs")]
    #[schemars(schema_with = "signs_schema")]
    pub binary_signs: Vec<SignConfig>,
    /// Docker image signing configurations.
    pub docker_signs: Option<Vec<DockerSignConfig>>,
    // No `alias` attribute needed: unlike `signs`/`sign`, "upx" is already
    // both singular and plural, so a separate alias adds no value.
    /// UPX binary compression configurations.
    #[serde(default, deserialize_with = "deserialize_upx")]
    #[schemars(schema_with = "upx_schema")]
    pub upx: Vec<UpxConfig>,
    /// Snapshot release configuration (local/non-tag builds).
    pub snapshot: Option<SnapshotConfig>,
    /// Nightly release configuration.
    pub nightly: Option<NightlyConfig>,
    /// Announcement configuration (Slack, Discord, email, etc.).
    pub announce: Option<AnnounceConfig>,
    /// When true, log artifact file sizes after building.
    pub report_sizes: Option<bool>,
    /// Environment variables available to all template expressions.
    ///
    /// List of `KEY=VALUE` strings (matches GoReleaser):
    /// `env: ["MY_VAR=hello", "DEPLOY_ENV=staging"]`. Order is preserved so
    /// chained env applications (sign + sbom + notarize) see entries in
    /// declared order. Values are rendered through the template engine before
    /// being set, so expressions like `{{ .Tag }}` or `{{ .Date }}` are
    /// expanded.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Custom template variables accessible as `{{ .Var.<key> }}` in templates.
    /// Provides a way to define reusable values, especially useful with config includes.
    ///
    /// Stored as a `BTreeMap` so rendering iterates in deterministic
    /// (sorted) key order — without this guarantee, a value that references
    /// another variable (`b: "{{ .Var.a }}_v2"`) could render before its
    /// dependency on a different process / host. The current resolver is
    /// single-pass (one render per value), so cross-variable references
    /// only resolve when the referenced key sorts earlier.
    pub variables: Option<BTreeMap<String, String>>,
    /// Generic artifact publisher configurations.
    pub publishers: Option<Vec<PublisherConfig>>,
    /// DockerHub description sync configurations.
    pub dockerhub: Option<Vec<DockerHubConfig>>,
    /// Artifactory upload configurations.
    pub artifactories: Option<Vec<ArtifactoryConfig>>,
    /// CloudSmith publisher configurations.
    pub cloudsmiths: Option<Vec<CloudSmithConfig>>,
    /// Top-level Homebrew Cask configurations.
    /// `homebrew_casks` is a top-level array with its own
    /// repository, commit_author, directory, skip_upload, hooks, dependencies,
    /// conflicts, completions, manpages, structured uninstall/zap, etc.
    pub homebrew_casks: Option<Vec<HomebrewCaskConfig>>,
    /// Automatic semantic version tagging configuration.
    pub tag: Option<TagConfig>,
    /// Git-level tag discovery and sorting settings.
    pub git: Option<GitConfig>,
    /// Partial/split build configuration for fan-out CI pipelines.
    pub partial: Option<PartialConfig>,
    /// Independent workspace roots in a monorepo.
    pub workspaces: Option<Vec<WorkspaceConfig>>,
    /// Source archive configuration.
    pub source: Option<SourceConfig>,
    /// Software bill of materials (SBOM) generation configurations.
    #[serde(default, deserialize_with = "deserialize_sboms")]
    #[schemars(schema_with = "sboms_schema")]
    pub sboms: Vec<SbomConfig>,
    /// GitHub release configuration shared by all crates.
    pub release: Option<ReleaseConfig>,
    /// Custom GitHub API/upload/download URLs for GitHub Enterprise installations.
    pub github_urls: Option<GitHubUrlsConfig>,
    /// Custom GitLab API/download URLs for self-hosted GitLab installations.
    pub gitlab_urls: Option<GitLabUrlsConfig>,
    /// Custom Gitea API/download URLs for self-hosted Gitea installations.
    pub gitea_urls: Option<GiteaUrlsConfig>,
    /// Force a specific token type for authentication.
    /// When set, overrides automatic token detection from environment variables.
    pub force_token: Option<ForceTokenKind>,
    /// macOS code signing and notarization configuration.
    pub notarize: Option<NotarizeConfig>,
    /// Project metadata configuration (applied to metadata.json output files).
    pub metadata: Option<MetadataConfig>,
    /// Template files to render and include as release artifacts.
    /// File contents are processed through the template engine.
    pub template_files: Option<Vec<TemplateFileConfig>>,
    /// GoReleaser Pro monorepo configuration.
    /// When configured, tag discovery filters by tag_prefix and the working
    /// directory is scoped to dir.
    pub monorepo: Option<MonorepoConfig>,
    /// Makeself self-extracting archive configurations.
    #[serde(default, deserialize_with = "deserialize_makeselfs")]
    #[schemars(schema_with = "makeselfs_schema")]
    pub makeselfs: Vec<MakeselfConfig>,
    /// Source RPM configuration. Renamed from `srpm:` (singular) for spelling
    /// parity with `Defaults.srpms` and the rest of the plural-name packaging
    /// fields. The `srpm:` spelling is still accepted via serde alias for
    /// back-compat.
    #[serde(alias = "srpm")]
    pub srpms: Option<SrpmConfig>,
    /// Milestone closing configurations.
    pub milestones: Option<Vec<MilestoneConfig>>,
    /// Generic HTTP upload configurations.
    pub uploads: Option<Vec<UploadConfig>>,
    /// AUR source package publishing configurations (source-only PKGBUILD, not -bin).
    pub aur_sources: Option<Vec<AurSourceConfig>>,
    /// Top-level retry configuration applied to network-bound operations
    /// (announcers, git providers, HTTP uploads, docker pipes). When omitted,
    /// `RetryConfig::default()` is used (10 attempts, 10s base, 5m cap —
    /// matching GoReleaser `Project.Retry`).
    pub retry: Option<RetryConfig>,
    /// MCP (Model Context Protocol) server registry publishing
    /// configuration. When `name` is empty (the default), the publisher is
    /// skipped. Mirrors GoReleaser's `mcp:` block.
    #[serde(default)]
    pub mcp: McpConfig,
    /// NPM package registry publishing configurations. One entry per
    /// published package. Mirrors GoReleaser Pro's `npms:` block.
    pub npms: Option<Vec<NpmConfig>>,
    /// GemFury (fury.io) deb/rpm/apk publishing configurations. Mirrors
    /// GoReleaser Pro's `gemfury:` block. The pre-GR-v2.14 spelling
    /// `furies:` is accepted via serde alias; a one-time deprecation
    /// warning is emitted by [`warn_on_legacy_furies_alias`].
    #[serde(alias = "furies")]
    pub gemfury: Option<Vec<GemFuryConfig>>,
}

/// Helper schema function for the signs field (accepts object or array).
fn signs_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<SignConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some("Artifact signing configurations (cosign, GPG, etc.). Accepts a single object or array.".to_owned());
    }
    schema
}

/// Helper schema function for the upx field (accepts object or array).
fn upx_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<UpxConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some(
            "UPX binary compression configurations. Accepts a single object or array.".to_owned(),
        );
    }
    schema
}

/// Helper schema function for the sboms field (accepts object or array).
fn sboms_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<SbomConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description =
            Some("SBOM generation configurations. Accepts a single object or array.".to_owned());
    }
    schema
}

fn default_dist() -> PathBuf {
    PathBuf::from("./dist")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: None,
            project_name: String::new(),
            dist: default_dist(),
            includes: None,
            env_files: None,
            defaults: None,
            before: None,
            after: None,
            before_publish: None,
            crates: Vec::new(),
            changelog: None,
            signs: Vec::new(),
            binary_signs: Vec::new(),
            docker_signs: None,
            upx: Vec::new(),
            snapshot: None,
            nightly: None,
            announce: None,
            report_sizes: None,
            env: None,
            variables: None,
            publishers: None,
            dockerhub: None,
            artifactories: None,
            cloudsmiths: None,
            homebrew_casks: None,
            tag: None,
            git: None,
            partial: None,
            workspaces: None,
            source: None,
            sboms: Vec::new(),
            release: None,
            github_urls: None,
            gitlab_urls: None,
            gitea_urls: None,
            force_token: None,
            notarize: None,
            metadata: None,
            template_files: None,
            monorepo: None,
            makeselfs: Vec::new(),
            srpms: None,
            milestones: None,
            uploads: None,
            aur_sources: None,
            retry: None,
            mcp: McpConfig::default(),
            npms: None,
            gemfury: None,
        }
    }
}

impl Config {
    /// Return the monorepo tag prefix, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())`.
    pub fn monorepo_tag_prefix(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.tag_prefix.as_deref())
    }

    /// Return the monorepo working directory, if configured.
    ///
    /// Shorthand for `config.monorepo.as_ref().and_then(|m| m.dir.as_deref())`.
    pub fn monorepo_dir(&self) -> Option<&str> {
        self.monorepo.as_ref().and_then(|m| m.dir.as_deref())
    }

    // --- Project metadata defaulting helpers (GoReleaser Pro parity) ---
    //
    // Publishers that expose homepage/license/description/maintainer fields
    // should fall back to these when their own field is unset, so a project
    // only needs to declare metadata once. Pattern:
    //
    //   let homepage = nfpm_cfg.homepage
    //       .as_deref()
    //       .or_else(|| cfg.meta_homepage());
    //
    // Returns None if the `metadata` section is missing or the field is unset.

    /// Project homepage from `metadata.homepage` (Pro default source for publishers).
    pub fn meta_homepage(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.homepage.as_deref())
    }

    /// Project license from `metadata.license`.
    pub fn meta_license(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.license.as_deref())
    }

    /// Project description from `metadata.description`.
    pub fn meta_description(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.description.as_deref())
    }

    /// Project maintainers from `metadata.maintainers`.
    pub fn meta_maintainers(&self) -> &[String] {
        self.metadata
            .as_ref()
            .and_then(|m| m.maintainers.as_deref())
            .unwrap_or(&[])
    }

    /// First maintainer as "Name <email>" or just "Name" (publisher convention).
    /// Returns None when no maintainers are configured.
    pub fn meta_first_maintainer(&self) -> Option<&str> {
        self.meta_maintainers().first().map(|s| s.as_str())
    }

    /// `true` when any top-level / workspace `signs:` or `binary_signs:`
    /// entry will invoke gpg (via `SignConfig::is_gpg()`).
    ///
    /// Used by preflight to decide whether to probe
    /// `gpg --faked-system-time` support. `docker_signs:` is excluded
    /// because that driver only ever invokes cosign.
    pub fn has_gpg_sign_configured(&self) -> bool {
        let top_level = self
            .signs
            .iter()
            .chain(self.binary_signs.iter())
            .any(|s| s.is_gpg());
        if top_level {
            return true;
        }
        // Workspaces inherit their own signs:/binary_signs: lists.
        self.workspaces.iter().flatten().any(|w| {
            w.signs
                .iter()
                .chain(w.binary_signs.iter())
                .any(|s| s.is_gpg())
        })
    }
}

/// Run a deserialization closure on a worker thread sized large enough that
/// the `Config` derive (60+ `Option<NestedStruct>` fields) cannot exhaust
/// the host's main-thread stack.
///
/// Background: debug builds of `serde_yaml_ng::from_value::<Config>` and
/// `toml::from_str::<Config>` consume several MiB of stack because each
/// generated visitor branch for the giant struct lives in a single
/// monomorphised frame and debug builds neither inline nor tail-call. The
/// Windows main-thread default reservation is 1 MiB, so any debug-built
/// integration test that triggers full-config deserialization overflows
/// before reaching the visitor's body.
///
/// Routing every full-`Config` deserialization through this helper keeps
/// every entry-point platform-agnostic without resorting to per-platform
/// linker flags or `RUST_MIN_STACK`.
pub fn deserialize_on_worker<F, T>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    use anyhow::Context as _;

    // 8 MiB matches the Linux/macOS process default and comfortably exceeds
    // the ~2 MiB peak observed for debug `Config` deserialization.
    const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .stack_size(WORKER_STACK_SIZE)
        .name("anodizer-config-deserialize".to_string())
        .spawn(f)
        .context("failed to spawn config deserialization worker thread")?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Validate the config schema version. Accepts version 1 (default) and 2.
/// Returns an error for unknown versions.
pub fn validate_version(config: &Config) -> Result<(), String> {
    match config.version {
        None | Some(1) | Some(2) => Ok(()),
        Some(v) => Err(format!(
            "unsupported config version: {}. Supported versions are 1 and 2.",
            v
        )),
    }
}

/// Validate `git.tag_sort` if present. Accepted values:
/// - `"-version:refname"` (default, lexicographic version sort)
/// - `"-version:creatordate"` (sort by tag creation date, newest first)
/// - `"semver"` (Rust-side strict SemVer 2.0.0 ordering, prereleases sort
///   below their release per spec section 11)
/// - `"smartsemver"` (same ordering as `semver`, but when the current version
///   is non-prerelease, prerelease tags are skipped when picking the previous
///   tag — avoids selecting `v0.2.0-beta.3` as the predecessor of `v0.2.0`)
///
/// Returns an error for unrecognized values.
pub fn validate_tag_sort(config: &Config) -> Result<(), String> {
    if let Some(ref git) = config.git
        && let Some(ref sort) = git.tag_sort
    {
        match sort.as_str() {
            "-version:refname" | "-version:creatordate" | "semver" | "smartsemver" => {}
            other => {
                return Err(format!(
                    "unsupported git.tag_sort value: \"{}\". \
                     Accepted values: \"-version:refname\", \"-version:creatordate\", \
                     \"semver\", \"smartsemver\".",
                    other
                ));
            }
        }
    }
    Ok(())
}

/// Known GOOS values accepted by `archives[].format_overrides[].goos`.
/// Mirrors the Go runtime's `runtime.GOOS` values GoReleaser's archive pipe
/// recognises; anything outside this set is almost always a typo
/// (e.g. a Rust target triple slice like `pc-windows-msvc`).
const KNOWN_GOOS: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "illumos",
    "ios",
    "js",
    "linux",
    "netbsd",
    "openbsd",
    "plan9",
    "solaris",
    "wasip1",
    "windows",
];

/// Validate that each crate's `release:` block configures at most one SCM
/// backend. Matches GoReleaser release.go:41-53 `ErrMultipleReleases`, which
/// errors at `Default()` time. Anodizer dispatches on `ctx.token_type` at
/// runtime so a silently-ignored extra backend is easy to miss.
pub fn validate_release_backends(config: &Config) -> Result<(), String> {
    let check = |crate_name: &str, release: &ReleaseConfig| -> Result<(), String> {
        let mut set = Vec::new();
        if release.github.is_some() {
            set.push("github");
        }
        if release.gitlab.is_some() {
            set.push("gitlab");
        }
        if release.gitea.is_some() {
            set.push("gitea");
        }
        if set.len() > 1 {
            return Err(format!(
                "crate {}: release config sets multiple mutually-exclusive SCM \
                 backends ({}). Pick one.",
                crate_name,
                set.join(" + ")
            ));
        }
        Ok(())
    };
    for krate in &config.crates {
        if let Some(ref release) = krate.release {
            check(&krate.name, release)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let Some(ref release) = krate.release {
                    check(&krate.name, release)?;
                }
            }
        }
    }
    Ok(())
}

/// Marker prefix for the axis-mismatch validation error class. Existing
/// validators in this module return `Result<(), String>` rather than a
/// typed enum, so we expose this constant (instead of a `ConfigError`
/// variant) for callers that want to recognise the error class
/// programmatically.
///
/// The prefix is emitted at the start of every error returned by
/// [`validate_defaults_axis`] (formatted as `"DefaultsAxisMismatch: …"`),
/// so callers can match with `err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH)`
/// or `err.contains(ERR_DEFAULTS_AXIS_MISMATCH)` without depending on the
/// exact human-readable wording.
///
/// ```ignore
/// match validate_defaults_axis(&config) {
///     Err(e) if e.starts_with(ERR_DEFAULTS_AXIS_MISMATCH) => {
///         // handle the axis-mismatch error class
///     }
///     other => other?,
/// }
/// ```
///
/// Future error-type unification can rename to
/// `ConfigError::DefaultsAxisMismatch` without changing call-sites that
/// match on this prefix.
pub const ERR_DEFAULTS_AXIS_MISMATCH: &str = "DefaultsAxisMismatch";

/// Validate that `defaults.crates:` and `defaults.workspaces:` match the
/// top-level axis.
///
/// Rules:
/// - `defaults.crates:` is set → top-level `crates:` MUST be present.
/// - `defaults.workspaces:` is set → top-level `workspaces:` MUST be present.
/// - Both `defaults.crates` and `defaults.workspaces` set simultaneously → error
///   (mutually exclusive).
/// - Wrong-axis (e.g. `defaults.crates:` while top-level uses `workspaces:`) → error.
pub fn validate_defaults_axis(config: &Config) -> Result<(), String> {
    let Some(ref defaults) = config.defaults else {
        return Ok(());
    };
    let has_crate_block = defaults.crates.is_some();
    let has_workspace_block = defaults.workspaces.is_some();

    if has_crate_block && has_workspace_block {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates and defaults.workspaces are \
             mutually exclusive — pick the axis that matches the top-level config \
             (`crates:` or `workspaces:`)",
        ));
    }

    let top_uses_workspaces = config.workspaces.as_ref().is_some_and(|w| !w.is_empty());
    let top_uses_crates = !config.crates.is_empty();

    if has_crate_block && !top_uses_crates {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.crates is set but top-level `crates:` \
             is {}; move defaults under `defaults.workspaces:` or remove the block",
            if top_uses_workspaces {
                "absent (top-level uses `workspaces:`)"
            } else {
                "absent"
            },
        ));
    }
    if has_workspace_block && !top_uses_workspaces {
        return Err(format!(
            "{ERR_DEFAULTS_AXIS_MISMATCH}: defaults.workspaces is set but top-level \
             `workspaces:` is {}; move defaults under `defaults.crates:` or remove the block",
            if top_uses_crates {
                "absent (top-level uses `crates:`)"
            } else {
                "absent"
            },
        ));
    }

    Ok(())
}

/// Validate `archives[].format_overrides[].goos` values reject unknown OSes.
/// GoReleaser silently no-ops unknown overrides, which has burned users typing
/// Rust triples like `apple` or `pc-windows-msvc`.
///
/// Walks every `archives[]` location in the config:
/// - `crates[].archives:`
/// - `workspaces[].crates[].archives:`
/// - `defaults.archives:` (an unknown `os` here would otherwise pass silently
///   and propagate to every inheriting crate at merge time).
pub fn validate_format_overrides(config: &Config) -> Result<(), String> {
    let check = |location: &str, archives: &[ArchiveConfig]| -> Result<(), String> {
        for (idx, archive) in archives.iter().enumerate() {
            let Some(ref overrides) = archive.format_overrides else {
                continue;
            };
            for over in overrides {
                if !KNOWN_GOOS.contains(&over.os.as_str()) {
                    let archive_id = archive.id.as_deref().unwrap_or("default");
                    return Err(format!(
                        "{}: archives[{}] (id={}): format_overrides.goos=\"{}\" is not a recognised OS. \
                         Accepted values: {}.",
                        location,
                        idx,
                        archive_id,
                        over.os,
                        KNOWN_GOOS.join(", ")
                    ));
                }
            }
        }
        Ok(())
    };
    for krate in &config.crates {
        if let ArchivesConfig::Configs(ref list) = krate.archives {
            check(&format!("crate {}", krate.name), list)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let ArchivesConfig::Configs(ref list) = krate.archives {
                    check(&format!("crate {}", krate.name), list)?;
                }
            }
        }
    }
    if let Some(ref defaults) = config.defaults
        && let Some(ref archive) = defaults.archives
    {
        // defaults.archives is a single ArchiveConfig (not a list); wrap it
        // into a one-element slice so the same checker walks it.
        check("defaults.archives", std::slice::from_ref(archive))?;
    }
    Ok(())
}

/// Validate that no [`HomebrewCaskConfig`] sets both `url_template` AND
/// `url.template` simultaneously — they are mutually exclusive shorthands
/// for the same URL field and combining them is ambiguous.
///
/// Inspects every occurrence of `HomebrewCaskConfig` in the config:
/// - `homebrew_casks:` (top-level array)
/// - `crates[].publish.homebrew_cask:`
/// - `workspaces[].crates[].publish.homebrew_cask:`
/// - `defaults.publish.homebrew_cask:`
pub fn validate_homebrew_cask_url_template(config: &Config) -> Result<(), String> {
    let check = |location: &str, cask: &HomebrewCaskConfig| -> Result<(), String> {
        let has_url_template = cask.url_template.is_some();
        let has_url_dot_template = cask.url.as_ref().is_some_and(|u| u.template.is_some());
        if has_url_template && has_url_dot_template {
            return Err(format!(
                "{location}: homebrew_cask sets both `url_template` and `url.template`. \
                 These are mutually exclusive — use one or the other."
            ));
        }
        Ok(())
    };

    // Top-level homebrew_casks array
    if let Some(ref casks) = config.homebrew_casks {
        for (i, cask) in casks.iter().enumerate() {
            check(&format!("homebrew_casks[{i}]"), cask)?;
        }
    }

    // Per-crate publish.homebrew_cask
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish
            && let Some(ref cask) = publish.homebrew_cask
        {
            check(
                &format!("crates[{}].publish.homebrew_cask", krate.name),
                cask,
            )?;
        }
    }

    // Workspace crates
    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish
                    && let Some(ref cask) = publish.homebrew_cask
                {
                    check(
                        &format!(
                            "workspaces[{}].crates[{}].publish.homebrew_cask",
                            ws.name, krate.name
                        ),
                        cask,
                    )?;
                }
            }
        }
    }

    // defaults.publish.homebrew_cask
    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
        && let Some(ref cask) = publish.homebrew_cask
    {
        check("defaults.publish.homebrew_cask", cask)?;
    }

    Ok(())
}

/// Validate that `archives[].id` and `universal_binaries[].id` are unique
/// within their respective lists.
///
/// Mirrors GoReleaser's `ids.New("archives").Inc(...).Validate()` pattern in
/// `internal/pipe/archive/archive.go:56-102` and the equivalent
/// `internal/pipe/universalbinary/universalbinary.go:36-50`. Two archive
/// configs with the same `id` silently both set the same `id` metadata key
/// on artifacts, breaking publishers that filter `ids: [<id>]`. Anodizer's
/// build/sign stages already enforce id uniqueness; archive and
/// universal_binary were missed.
///
/// Walks every occurrence of `archives[]` and `universal_binaries[]`:
/// - `crates[].archives:` / `crates[].universal_binaries:`
/// - `workspaces[].crates[].archives:` / `.universal_binaries:`
/// - `defaults.archives:` is a single `ArchiveConfig`, so uniqueness within
///   itself is vacuously true; not walked here.
///
pub fn validate_id_uniqueness(config: &Config) -> Result<(), String> {
    fn check_unique<F>(
        location: &str,
        kind: &str,
        ids: impl IntoIterator<Item = (usize, Option<String>)>,
        empty_ok: F,
    ) -> Result<(), String>
    where
        F: Fn() -> bool,
    {
        let _ = empty_ok;
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (idx, maybe_id) in ids {
            // GoReleaser stores empty as "default" for archives via Default-time
            // assignment. Anodizer applies `default_archive_id` at deserialize
            // time, so the option is normally `Some("default")`. A truly empty
            // / None id here means the user explicitly cleared it; we still
            // dedupe across `None` so two None-id'd entries collide just like
            // two "default"-id'd entries would.
            let key = maybe_id.unwrap_or_else(|| "<unset>".to_string());
            if let Some(prev_idx) = seen.insert(key.clone(), idx) {
                return Err(format!(
                    "{location}: {kind} id \"{key}\" is used by both entry {prev_idx} and entry {idx} — \
                     ids must be unique within a {kind} list."
                ));
            }
        }
        Ok(())
    }

    let check_archives = |location: &str, archives: &[ArchiveConfig]| -> Result<(), String> {
        check_unique(
            location,
            "archives",
            archives.iter().enumerate().map(|(i, a)| (i, a.id.clone())),
            || true,
        )
    };
    let check_unibins = |location: &str, ubs: &[UniversalBinaryConfig]| -> Result<(), String> {
        check_unique(
            location,
            "universal_binaries",
            ubs.iter().enumerate().map(|(i, u)| (i, u.id.clone())),
            || true,
        )
    };

    for krate in &config.crates {
        if let ArchivesConfig::Configs(ref list) = krate.archives {
            check_archives(&format!("crates[{}].archives", krate.name), list)?;
        }
        if let Some(ref ubs) = krate.universal_binaries {
            check_unibins(&format!("crates[{}].universal_binaries", krate.name), ubs)?;
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                if let ArchivesConfig::Configs(ref list) = krate.archives {
                    check_archives(
                        &format!("workspaces[{}].crates[{}].archives", ws.name, krate.name),
                        list,
                    )?;
                }
                if let Some(ref ubs) = krate.universal_binaries {
                    check_unibins(
                        &format!(
                            "workspaces[{}].crates[{}].universal_binaries",
                            ws.name, krate.name
                        ),
                        ubs,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Validate `builds[]` entries that opt into `builder: prebuilt`.
///
/// `builder: prebuilt` skips `cargo build` and imports a binary the
/// operator staged elsewhere. The validation rules below mirror GoReleaser's
/// `prebuilt` builder contract (`/customization/builds/builders/prebuilt.md`):
///
/// 1. `prebuilt:` block MUST be set and `prebuilt.path` MUST be non-empty.
/// 2. `targets:` MUST be explicit on the build entry — no `defaults.targets`
///    fallback. Without this rule the build matrix has no rows.
/// 3. Cargo-only knobs are rejected as mutually exclusive: `cross_tool`,
///    `features`, `no_default_features`, `command`. The crate-level
///    `cross:` strategy is also rejected when any build on the crate is
///    prebuilt (the strategy has no meaning when nothing is being
///    compiled).
/// 4. `builder: cargo` (the default) with a `prebuilt:` block set warns —
///    the block has no effect and likely indicates a forgotten
///    `builder: prebuilt`.
pub fn validate_builds(config: &Config) -> Result<(), String> {
    let check_crate = |location: &str, krate: &CrateConfig| -> Result<(), String> {
        let Some(ref builds) = krate.builds else {
            return Ok(());
        };
        let crate_is_prebuilt = builds
            .iter()
            .any(|b| matches!(b.builder, Some(BuilderKind::Prebuilt)));
        if crate_is_prebuilt && krate.cross.is_some() {
            return Err(format!(
                "{location}: crate-level `cross:` strategy is set but at least one \
                 build uses `builder: prebuilt`; remove `cross:` (prebuilt imports a \
                 binary instead of compiling) or change the build's builder to `cargo`."
            ));
        }
        for (idx, build) in builds.iter().enumerate() {
            match build.builder {
                Some(BuilderKind::Prebuilt) => {
                    let path = build.prebuilt.as_ref().map(|p| p.path.trim()).unwrap_or("");
                    if path.is_empty() {
                        return Err(format!(
                            "{location}.builds[{idx}]: `builder: prebuilt` requires a non-empty \
                             `prebuilt.path` template. Example: \
                             `prebuilt: {{ path: \"output/mybin_{{{{ .Target }}}}\" }}`"
                        ));
                    }
                    let targets_explicit = build.targets.as_ref().is_some_and(|t| !t.is_empty());
                    if !targets_explicit {
                        return Err(format!(
                            "{location}.builds[{idx}] has `builder: prebuilt` but no explicit \
                             `targets:` — the prebuilt builder requires per-build target triples \
                             (no `defaults.targets:` fallback). Add `targets: [<triple>, ...]`."
                        ));
                    }
                    if build.cross_tool.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `cross_tool` is set with \
                             `builder: prebuilt` — the two are mutually exclusive. \
                             `cross_tool` controls how cargo cross-compiles; `prebuilt` \
                             imports an already-built binary. Drop `cross_tool` or use \
                             `builder: cargo`."
                        ));
                    }
                    if build.command.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `command:` override is set with \
                             `builder: prebuilt` — the override selects the cargo \
                             subcommand, which is not invoked under the prebuilt \
                             builder. Drop `command:` or use `builder: cargo`."
                        ));
                    }
                    if build.features.as_ref().is_some_and(|f| !f.is_empty()) {
                        return Err(format!(
                            "{location}.builds[{idx}]: `features:` is set with \
                             `builder: prebuilt` — Cargo features are evaluated at \
                             compile time, which the prebuilt builder skips. \
                             Drop `features:` or use `builder: cargo`."
                        ));
                    }
                    if build.no_default_features.is_some() {
                        return Err(format!(
                            "{location}.builds[{idx}]: `no_default_features:` is set with \
                             `builder: prebuilt` — Cargo feature flags are evaluated at \
                             compile time, which the prebuilt builder skips. \
                             Drop the flag or use `builder: cargo`."
                        ));
                    }
                }
                Some(BuilderKind::Cargo) | None => {
                    if build.prebuilt.is_some() {
                        tracing::warn!(
                            location = %location,
                            index = idx,
                            "build has a `prebuilt:` block but `builder:` is not `prebuilt`; \
                             the block is ignored. Set `builder: prebuilt` or remove the block.",
                        );
                    }
                }
            }
        }
        Ok(())
    };

    for krate in &config.crates {
        check_crate(&format!("crates[{}]", krate.name), krate)?;
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                check_crate(
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    krate,
                )?;
            }
        }
    }
    Ok(())
}

/// Returns `true` if every build entry on every crate has
/// `builder: prebuilt`. Used by the determinism harness to short-circuit:
/// when no target compiles, there is nothing for the harness to rebuild
/// and compare across runs.
pub fn all_builds_prebuilt(config: &Config) -> bool {
    let crate_all_prebuilt = |krate: &CrateConfig| -> Option<bool> {
        let builds = krate.builds.as_ref()?;
        if builds.is_empty() {
            return None;
        }
        Some(
            builds
                .iter()
                .all(|b| matches!(b.builder, Some(BuilderKind::Prebuilt))),
        )
    };

    let mut saw_any = false;
    for krate in &config.crates {
        match crate_all_prebuilt(krate) {
            Some(true) => saw_any = true,
            Some(false) => return false,
            None => {}
        }
    }
    if let Some(ws_list) = config.workspaces.as_ref() {
        for ws in ws_list {
            for krate in &ws.crates {
                match crate_all_prebuilt(krate) {
                    Some(true) => saw_any = true,
                    Some(false) => return false,
                    None => {}
                }
            }
        }
    }
    saw_any
}

/// Validate the depth of `changelog.groups[].groups`.
///
/// GoReleaser Pro caps subgroups at ONE level
/// (`/customization/publish/changelog.md`: "There can only be one level of
/// subgroups"). Anodizer's renderer can technically handle deeper nesting
/// (capped at 6 to match Markdown's heading limit), but accepting deeper
/// configs silently is a footgun: a config that works in anodizer but is
/// rejected by GR breaks parity for users migrating between the two.
///
/// Rejects any `changelog.groups[i].groups[j].groups[..]` configuration
/// with a clear error pointing at the offending parent group title.
pub fn validate_changelog_groups_depth(config: &Config) -> Result<(), String> {
    let check = |location: &str, cfg: &ChangelogConfig| -> Result<(), String> {
        let Some(ref groups) = cfg.groups else {
            return Ok(());
        };
        for g in groups {
            if let Some(ref subs) = g.groups {
                for sub in subs {
                    if sub.groups.as_ref().is_some_and(|s| !s.is_empty()) {
                        return Err(format!(
                            "{location}: changelog group '{}' > '{}' nests further \
                             subgroups; GoReleaser permits only one level of subgroups \
                             (see https://goreleaser.com/customization/changelog/). \
                             Flatten the inner groups into the parent or split into \
                             sibling top-level groups.",
                            g.title, sub.title
                        ));
                    }
                }
            }
        }
        Ok(())
    };
    if let Some(ref cfg) = config.changelog {
        check("changelog", cfg)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            if let Some(ref cfg) = ws.changelog {
                check(&format!("workspaces[{}].changelog", ws.name), cfg)?;
            }
        }
    }
    Ok(())
}

/// Validate `changelog.paths[]` syntax.
///
/// Path patterns are passed straight to `git log -- <path>` (or the
/// per-SCM equivalent). Two patterns are always wrong:
/// - Leading `/` — git pathspec treats this as anchored-to-CWD which is
///   almost never what the user wrote and produces empty changelogs.
/// - Empty string — silently matches everything; rejected so a typo
///   doesn't disable filtering.
///
/// Globs containing `**` are accepted (git accepts them) but the docs
/// note their semantics differ from gitignore; that's a docs concern,
/// not a hard error.
pub fn validate_changelog_paths(config: &Config) -> Result<(), String> {
    let check = |location: &str, cfg: &ChangelogConfig| -> Result<(), String> {
        let Some(ref paths) = cfg.paths else {
            return Ok(());
        };
        for (idx, p) in paths.iter().enumerate() {
            if p.is_empty() {
                return Err(format!(
                    "{location}: changelog.paths[{idx}] is empty; remove the entry \
                     or set a real path (empty string matches everything and \
                     disables filtering)"
                ));
            }
            if p.starts_with('/') {
                return Err(format!(
                    "{location}: changelog.paths[{idx}] = {:?} starts with '/'; \
                     git pathspec is repo-root-relative — write {:?} instead",
                    p,
                    p.trim_start_matches('/')
                ));
            }
        }
        Ok(())
    };
    if let Some(ref cfg) = config.changelog {
        check("changelog", cfg)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            if let Some(ref cfg) = ws.changelog {
                check(&format!("workspaces[{}].changelog", ws.name), cfg)?;
            }
        }
    }
    Ok(())
}

/// Emit a `tracing::warn!` for each publisher configured with `required: true`
/// whose group is Submitter (chocolatey, winget, aur_source).
///
/// Submitter publishers push to external moderation queues that do not resolve
/// within a release window, so `required: true` never has the desired effect
/// of blocking the release on approval. The warning is non-fatal — the user
/// may have private-registry semantics where resolution is fast. Cargo is
/// excluded: its default is already `required: true` and the warning would
/// be noise.
pub fn warn_on_submitter_required(config: &Config) {
    for msg in submitter_required_warnings(config) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns the warning strings without emitting them. Exposed
/// for tests; production callers use [`warn_on_submitter_required`].
pub(crate) fn submitter_required_warnings(config: &Config) -> Vec<String> {
    fn submitter_warning(location: &str, name: &str) -> String {
        format!(
            "{location}: publisher '{name}' is a submitter (external moderation queue); \
             `required: true` has no meaningful effect — the submitter gate \
             evaluates at push time, not at approval time."
        )
    }

    let mut warnings = Vec::new();

    for krate in &config.crates {
        if let Some(ref publish) = krate.publish {
            let loc = format!("crate '{}'", krate.name);
            if publish.chocolatey.as_ref().and_then(|c| c.required) == Some(true) {
                warnings.push(submitter_warning(&loc, "chocolatey"));
            }
            if publish.winget.as_ref().and_then(|w| w.required) == Some(true) {
                warnings.push(submitter_warning(&loc, "winget"));
            }
            if publish.aur_source.as_ref().and_then(|a| a.required) == Some(true) {
                warnings.push(submitter_warning(&loc, "aur_source"));
            }
        }
    }

    // Top-level aur_sources list (not nested under publish:) — no crate axis,
    // distinguish via the index in the list so two top-level entries collide cleanly.
    if let Some(ref sources) = config.aur_sources {
        for (idx, src) in sources.iter().enumerate() {
            if src.required == Some(true) {
                let loc = format!("top-level aur_sources[{idx}]");
                warnings.push(submitter_warning(&loc, "aur_source"));
            }
        }
    }

    warnings
}

/// No-op preserved for API stability; the legacy `format:` and `builds:`
/// folds happen inline in `<ArchiveConfig as Deserialize>::deserialize` and
/// `<FormatOverride as Deserialize>::deserialize`. Emits no warning of its
/// own — every alias hit was already announced at deserialize time.
///
pub fn apply_archive_legacy_aliases(_config: &mut Config) {
    // Intentionally empty — see Deserialize impls.
}

/// Reject the GoReleaser V1 `dockers:` block at config-load time with a
/// clear migration error.
///
/// anodizer is V2-only by design: it implements `docker_v2:` and the
/// associated multi-arch buildx flow, but does not ship the V1
/// `dockers: -> dockerfile + image_templates` pipe. Without this check the
/// top-level `Config` struct's `deny_unknown_fields` would emit a generic
/// "unknown field `dockers`" message that doesn't tell the user how to
/// migrate. This explicit error names the field, points at `docker_v2:`,
/// and references the rationale.
///
pub fn validate_no_docker_v1(raw_yaml: &serde_yaml_ng::Value) -> Result<(), String> {
    if raw_yaml.get("dockers").is_some() {
        return Err(
            "config: legacy GoReleaser `dockers:` block is not supported — anodizer ships \
             docker_v2: only (multi-arch buildx flow). Port the config to `docker_v2:` per \
             https://anodize.dev/docs/migration/docker.html."
                .to_string(),
        );
    }
    Ok(())
}

/// Emit a `tracing::warn!` for each `publish.homebrew:` (Homebrew Formula)
/// occurrence in the loaded config. GoReleaser v2.16 deprecated the
/// Formula publisher in favour of `homebrew_casks:`; anodizer mirrors the
/// upstream deprecation so users following the GR change-log see the
/// same migration prompt.
///
/// Covers three placement axes (matching how `publish.homebrew` may appear):
///   * `crates[].publish.homebrew`
///   * `workspaces[].crates[].publish.homebrew`
///   * `defaults.publish.homebrew`
///
/// There is no top-level `homebrew:` or `brews:` field on anodizer's
/// `Config` — only `homebrew_casks:` lives at the top level — so this
/// function does not need a top-level scan.
pub fn warn_on_legacy_homebrew_formula(config: &Config) {
    for msg in legacy_homebrew_formula_warnings(config) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns the warning strings without emitting them.
/// Exposed for tests; production callers use
/// [`warn_on_legacy_homebrew_formula`].
pub(crate) fn legacy_homebrew_formula_warnings(config: &Config) -> Vec<String> {
    fn formula_warning(location: &str) -> String {
        format!(
            "DEPRECATION: {location}: publish.homebrew (Homebrew Formula) is deprecated upstream \
             in GoReleaser v2.16; migrate to homebrew_casks. Cask is now the canonical Homebrew \
             distribution channel for pre-compiled binaries. See \
             https://anodize.dev/docs/publish/homebrew-casks/ for migration."
        )
    }

    let mut warnings = Vec::new();

    for krate in &config.crates {
        if let Some(ref publish) = krate.publish
            && publish.homebrew.is_some()
        {
            warnings.push(formula_warning(&format!("crate '{}'", krate.name)));
        }
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish
                    && publish.homebrew.is_some()
                {
                    warnings.push(formula_warning(&format!(
                        "workspaces[{}].crates[{}]",
                        ws.name, krate.name
                    )));
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
        && publish.homebrew.is_some()
    {
        warnings.push(formula_warning("defaults.publish"));
    }

    warnings
}

/// Fold the deprecated `snapshot.name_template` alias into `version_template`.
/// Serde already accepts both spellings via `#[serde(alias = "name_template")]`,
/// so this function only needs to emit the deprecation warning when the
/// raw YAML key was the legacy one.
///
/// Because serde collapses the two spellings to a single field on parse, we
/// lose the information about which key the user wrote. This function
/// therefore consults the raw YAML pre-parse value (when supplied) to decide.
pub fn warn_on_legacy_snapshot_name_template(raw_yaml: &serde_yaml_ng::Value) {
    if let Some(snap) = raw_yaml.get("snapshot")
        && snap.get("name_template").is_some()
    {
        tracing::warn!(
            "DEPRECATION: snapshot.name_template is deprecated; use \
             snapshot.version_template instead. Both spellings are accepted \
             but the legacy key will be removed in a future release."
        );
    }
}

/// Emit a one-time deprecation warning when a config uses the legacy
/// `furies:` top-level key. Serde transparently folds `furies:` into
/// `gemfury:` via `#[serde(alias)]`, so this function consults the raw YAML
/// pre-parse value to detect the legacy spelling.
///
/// Matches GoReleaser Pro v2.14's `furies → gemfury` rename messaging.
pub fn warn_on_legacy_furies_alias(raw_yaml: &serde_yaml_ng::Value) {
    if raw_yaml.get("furies").is_some() {
        tracing::warn!(
            "DEPRECATION: the top-level `furies:` config key is deprecated since GoReleaser \
             Pro v2.14; rename it to `gemfury:`. Both spellings are accepted but the legacy \
             key will be removed in a future release."
        );
    }
}

/// Reject the GoReleaser pre-v2.13.1 nested `mcp.github:` block with a
/// clear migration error.
///
/// GR v2.13.1 flattened the registry metadata that used to live under
/// `mcp.github:` (repository owner/name/url) to the top-level `mcp:` block
/// (canonical surface: `mcp.repository:`, `mcp.name:`, etc.). Anodizer
/// never carried the nested shim — its `McpConfig` has `deny_unknown_fields`
/// so the key would otherwise produce a generic "unknown field" message.
/// This pre-parse check intercepts the legacy spelling so the user sees a
/// migration pointer rather than a schema-shape error.
pub fn validate_no_mcp_github(raw_yaml: &serde_yaml_ng::Value) -> Result<(), String> {
    if raw_yaml.get("mcp").and_then(|m| m.get("github")).is_some() {
        return Err(
            "config: nested `mcp.github:` block is not supported — anodizer mirrors GoReleaser \
             v2.13.1+ where registry metadata moved to top-level `mcp:` fields (`mcp.name`, \
             `mcp.repository.url`, `mcp.repository.source`). Port the nested keys to the \
             canonical surface."
                .to_string(),
        );
    }
    Ok(())
}

/// Emit a one-time deprecation warning for each `dockers_v2[].retry:` or
/// `docker_manifests[].retry:` block at config-load time. The per-pipe
/// `retry:` field is the legacy shape (GR v2.15.3 moved retry handling to
/// the top-level `retry:` block); the per-pipe value is still honored at
/// resolve-time (see `stage-docker::resolve_retry_params`) but a top-level
/// `retry:` is the canonical surface for retry policy. Warning fires once
/// per occurrence so users porting from older GR configs see a clear
/// pointer at load time without waiting for the docker pipe to execute.
pub fn warn_on_legacy_docker_retry(config: &Config) {
    for msg in legacy_docker_retry_warnings(config) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns the warning strings without emitting them. Exposed
/// for tests; production callers use [`warn_on_legacy_docker_retry`].
pub(crate) fn legacy_docker_retry_warnings(config: &Config) -> Vec<String> {
    fn pipe_warning(location: &str, kind: &str) -> String {
        format!(
            "DEPRECATION: {location}: nested `{kind}.retry:` is deprecated since GoReleaser \
             v2.15.3; move retry settings to the top-level `retry:` block. The per-pipe \
             value still wins at resolve time for back-compat, but the legacy spelling will \
             be removed in a future release."
        )
    }

    let mut warnings = Vec::new();

    let scan_crate = |krate: &CrateConfig, prefix: &str, warnings: &mut Vec<String>| {
        if let Some(ref v2) = krate.docker_v2 {
            for (i, cfg) in v2.iter().enumerate() {
                if cfg.retry.is_some() {
                    warnings.push(pipe_warning(
                        &format!("{prefix}.docker_v2[{i}]"),
                        "docker_v2",
                    ));
                }
            }
        }
        if let Some(ref manifests) = krate.docker_manifests {
            for (i, cfg) in manifests.iter().enumerate() {
                if cfg.retry.is_some() {
                    warnings.push(pipe_warning(
                        &format!("{prefix}.docker_manifests[{i}]"),
                        "docker_manifests",
                    ));
                }
            }
        }
    };

    for krate in &config.crates {
        scan_crate(krate, &format!("crates[{}]", krate.name), &mut warnings);
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                scan_crate(
                    krate,
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    &mut warnings,
                );
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref v2) = defaults.docker_v2
        && v2.retry.is_some()
    {
        warnings.push(pipe_warning("defaults.docker_v2", "docker_v2"));
    }

    warnings
}

/// Fold the deprecated singular `binary:` field on Homebrew Cask configs
/// into the canonical plural [`HomebrewCaskConfig::binaries`] list and emit
/// a deprecation warning. GoReleaser v2.12.6 renamed the field from
/// `binary: <name>` to `binaries: [<name>]`; anodizer accepts both for
/// back-compat with imported configs.
///
/// When both spellings are present the legacy entry is prepended so the
/// user's explicit ordering in `binaries:` is preserved at the tail. The
/// captured value is moved out of [`HomebrewCaskConfig::legacy_binary`] so
/// downstream code only ever reads the canonical field.
pub fn apply_homebrew_cask_legacy_binary(config: &mut Config) {
    fn fold_one(location: &str, cask: &mut HomebrewCaskConfig) -> Option<String> {
        let legacy = cask.legacy_binary.take()?;
        let entry = HomebrewCaskBinary::Name(legacy.clone());
        match cask.binaries {
            Some(ref mut list) => list.insert(0, entry),
            None => cask.binaries = Some(vec![entry]),
        }
        Some(format!(
            "DEPRECATION: {location}: singular `binary: {legacy}` is deprecated since \
             GoReleaser v2.12.6; use the plural `binaries: [{legacy}]` form. The legacy \
             value has been folded into binaries[0]."
        ))
    }

    let mut warnings = Vec::new();

    if let Some(ref mut casks) = config.homebrew_casks {
        for (i, cask) in casks.iter_mut().enumerate() {
            if let Some(msg) = fold_one(&format!("homebrew_casks[{i}]"), cask) {
                warnings.push(msg);
            }
        }
    }

    for krate in &mut config.crates {
        if let Some(ref mut publish) = krate.publish
            && let Some(ref mut cask) = publish.homebrew_cask
            && let Some(msg) = fold_one(
                &format!("crates[{}].publish.homebrew_cask", krate.name),
                cask,
            )
        {
            warnings.push(msg);
        }
    }

    if let Some(ref mut workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &mut ws.crates {
                if let Some(ref mut publish) = krate.publish
                    && let Some(ref mut cask) = publish.homebrew_cask
                    && let Some(msg) = fold_one(
                        &format!(
                            "workspaces[{}].crates[{}].publish.homebrew_cask",
                            ws.name, krate.name
                        ),
                        cask,
                    )
                {
                    warnings.push(msg);
                }
            }
        }
    }

    if let Some(ref mut defaults) = config.defaults
        && let Some(ref mut publish) = defaults.publish
        && let Some(ref mut cask) = publish.homebrew_cask
        && let Some(msg) = fold_one("defaults.publish.homebrew_cask", cask)
    {
        warnings.push(msg);
    }

    for msg in warnings {
        tracing::warn!("{}", msg);
    }
}

/// Emit a deprecation warning for any `builds[].gobinary` field. The field
/// is captured by [`BuildConfig::legacy_gobinary`] purely for back-compat
/// YAML import; anodizer's tool is always `cargo` so the value is unused.
pub fn apply_build_legacy_aliases(config: &mut Config) {
    let warn_one = |location: &str, legacy: &mut Option<String>| {
        if let Some(go_bin) = legacy.take() {
            tracing::warn!(
                "DEPRECATION: {location}: 'gobinary: {go_bin}' is a Go-only field; anodizer \
                 builds with cargo unconditionally. The value has been ignored."
            );
        }
    };
    for krate in &mut config.crates {
        if let Some(ref mut builds) = krate.builds {
            for (i, b) in builds.iter_mut().enumerate() {
                warn_one(
                    &format!("crates[{}].builds[{i}]", krate.name),
                    &mut b.legacy_gobinary,
                );
            }
        }
    }
    if let Some(ref mut workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &mut ws.crates {
                if let Some(ref mut builds) = krate.builds {
                    for (i, b) in builds.iter_mut().enumerate() {
                        warn_one(
                            &format!("workspaces[{}].crates[{}].builds[{i}]", ws.name, krate.name),
                            &mut b.legacy_gobinary,
                        );
                    }
                }
            }
        }
    }
    if let Some(ref mut defaults) = config.defaults
        && let Some(ref mut b) = defaults.builds
    {
        warn_one("defaults.builds", &mut b.legacy_gobinary);
    }
}

// ---------------------------------------------------------------------------
// EnvFilesConfig — accepts list of .env paths OR structured token file paths
// ---------------------------------------------------------------------------

mod env_files;
pub use env_files::*;

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

mod defaults;
pub use defaults::*;

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

mod build;
pub use build::*;

// ---------------------------------------------------------------------------
// ArchivesConfig — untagged enum: false => Disabled, array => Configs
// ---------------------------------------------------------------------------

mod archives;
pub use archives::*;

// ---------------------------------------------------------------------------
// ReleaseConfig
// ---------------------------------------------------------------------------

mod release;
pub use release::*;

// ---------------------------------------------------------------------------
// Shared publisher config types: RepositoryConfig, CommitAuthorConfig
// ---------------------------------------------------------------------------

mod publishers;
pub use publishers::*;

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

mod docker;
pub use docker::*;

// ---------------------------------------------------------------------------
// NfpmConfig
// ---------------------------------------------------------------------------

mod nfpm;
pub use nfpm::*;

// ---------------------------------------------------------------------------
// SnapcraftConfig
// ---------------------------------------------------------------------------

mod snapcraft;
pub use snapcraft::*;
// ---------------------------------------------------------------------------
// DmgConfig / MsiConfig / PkgConfig / NsisConfig / AppBundleConfig / FlatpakConfig
// ---------------------------------------------------------------------------

mod installers;
pub use installers::*;

// ---------------------------------------------------------------------------
// BlobConfig (S3/GCS/Azure cloud storage)
// ---------------------------------------------------------------------------

mod blob;
pub use blob::*;

// ---------------------------------------------------------------------------
// PartialConfig (split/merge CI fan-out)
// ---------------------------------------------------------------------------

mod partial;
pub use partial::*;

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

mod binstall;
pub use binstall::*;

// ---------------------------------------------------------------------------
// NotarizeConfig (macOS code signing and notarization)
// ---------------------------------------------------------------------------

mod notarize;
pub use notarize::*;
// ---------------------------------------------------------------------------
// SourceConfig
// ---------------------------------------------------------------------------

mod source;
pub use source::*;

// ---------------------------------------------------------------------------
// SbomConfig
// ---------------------------------------------------------------------------

mod sbom;
pub use sbom::*;

// ---------------------------------------------------------------------------
// VersionSyncConfig
// ---------------------------------------------------------------------------

mod version_sync;
pub use version_sync::*;

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

mod changelog;
pub use changelog::*;
// ---------------------------------------------------------------------------
// SignConfig / DockerSignConfig — lifted to `crate::signing`
// ---------------------------------------------------------------------------
//
// see `crate::signing` for the type definitions. The
// re-exports below preserve the historical
// `anodizer_core::config::{SignConfig, DockerSignConfig}` import paths
// used by every stage that consumes a sign config.

pub use crate::signing::{DockerSignConfig, SignConfig};

// ---------------------------------------------------------------------------
// UpxConfig
// ---------------------------------------------------------------------------

mod upx;
pub use upx::*;

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

mod snapshot_nightly;
pub use snapshot_nightly::*;

// ---------------------------------------------------------------------------
// TemplateFileConfig
// ---------------------------------------------------------------------------

mod templatefiles;
pub use templatefiles::*;

// ---------------------------------------------------------------------------
// AnnounceConfig
// ---------------------------------------------------------------------------
mod announce;
pub use announce::*;
// ---------------------------------------------------------------------------
// DockerHub description sync
// ---------------------------------------------------------------------------

mod dockerhub;
pub use dockerhub::*;

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

mod artifactory;
pub use artifactory::*;

// ---------------------------------------------------------------------------
// CloudSmith publisher
// ---------------------------------------------------------------------------

mod cloudsmith;
pub use cloudsmith::*;

// ---------------------------------------------------------------------------
// PublisherConfig
// ---------------------------------------------------------------------------

mod publisher;
pub use publisher::*;

// ---------------------------------------------------------------------------
// HooksConfig
// ---------------------------------------------------------------------------

mod hooks;
pub use hooks::*;

// ---------------------------------------------------------------------------
// GitConfig
// ---------------------------------------------------------------------------

mod git_config;
pub use git_config::*;

// ---------------------------------------------------------------------------
// MonorepoConfig
// ---------------------------------------------------------------------------

mod monorepo;
pub use monorepo::*;

// ---------------------------------------------------------------------------
// TagConfig
// ---------------------------------------------------------------------------

mod tag;
pub use tag::*;

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

mod workspace;
pub use workspace::*;

// ---------------------------------------------------------------------------
// RetryConfig (top-level `retry:` block — bridges to crate::retry::RetryPolicy)
// ---------------------------------------------------------------------------

mod retry;
pub use retry::*;

// ---------------------------------------------------------------------------
// PostPublishPollConfig (per-publisher post-publish polling)
// ---------------------------------------------------------------------------

mod post_publish_poll;
pub use post_publish_poll::*;

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

mod string_or_bool;
pub use string_or_bool::*;

// ---------------------------------------------------------------------------
// MakeselfConfig + SrpmConfig — lifted to `crate::packagers`
// ---------------------------------------------------------------------------
//
// All packaging config types live in their own modules under
// `crate::packagers`. The re-exports below preserve the historical
// `anodizer_core::config::{MakeselfConfig, MakeselfFile, SrpmConfig}`
// import paths used by stages and tests.

pub use crate::packagers::{MakeselfConfig, MakeselfFile, SrpmConfig};
pub(crate) use crate::packagers::{deserialize_makeselfs, makeselfs_schema};

// ---------------------------------------------------------------------------
// MilestoneConfig
// ---------------------------------------------------------------------------

mod milestone;
pub use milestone::*;

// ---------------------------------------------------------------------------
// UploadConfig (generic HTTP upload)
// ---------------------------------------------------------------------------

mod upload;
pub use upload::*;

// ---------------------------------------------------------------------------
// AurSourceConfig
// ---------------------------------------------------------------------------

mod aur_source;
pub use aur_source::*;

// ---------------------------------------------------------------------------
// McpConfig (MCP registry publisher)
// ---------------------------------------------------------------------------

mod mcp;
pub use mcp::*;

mod npm;
pub use npm::*;

// ---------------------------------------------------------------------------
// GemFuryConfig (Gemfury / fury.io publisher)
// ---------------------------------------------------------------------------

mod gemfury;
pub use gemfury::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
