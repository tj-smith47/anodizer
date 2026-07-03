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
///       path: ./config/release.yaml              # structured file path
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
#[serde(deny_unknown_fields)]
pub struct IncludeFilePath {
    /// Path to the include file (relative to the config file).
    pub path: String,
}

/// URL configuration for a structured include.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
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
/// fields at parse time (strict YAML unmarshalling).
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
    /// List of `KEY=VALUE` strings:
    /// `env: ["MY_VAR=hello", "DEPLOY_ENV=staging"]`. Order is preserved so
    /// chained env applications (sign + sbom + notarize) see entries in
    /// declared order. Values are rendered through the template engine before
    /// being set, so expressions like `{{ Tag }}` or `{{ Date }}` are
    /// expanded.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Custom template variables accessible as `{{ Var.<key> }}` in templates.
    /// Provides a way to define reusable values, especially useful with config includes.
    ///
    /// Stored as a `BTreeMap` so rendering iterates in deterministic
    /// (sorted) key order — without this guarantee, a value that references
    /// another variable (`b: "{{ Var.a }}_v2"`) could render before its
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
    /// Repo-committed files that embed the release version outside
    /// `Cargo.toml` (e.g. a Helm `Chart.yaml`, an install doc, a README
    /// badge), given as repo-root-relative path strings. At `tag` time each
    /// listed file has its occurrences of the old version rewritten to the new
    /// version — both the bare (`0.1.0`) and `v`-prefixed (`v0.1.0`) forms,
    /// word-boundary anchored — and is staged into the same bump commit as
    /// `Cargo.toml` / `Cargo.lock`, so these files never drift from the tag.
    ///
    /// ```yaml
    /// version_files:
    ///   - charts/cfgd/Chart.yaml
    ///   - docs/installation.md
    /// ```
    pub version_files: Option<Vec<String>>,
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
    /// SLSA build-provenance / attestation configuration for binaries and
    /// archives. In the default `subjects` mode, anodizer writes a subjects
    /// manifest for `actions/attest-build-provenance`; in `emit` mode it
    /// generates and signs a self-contained in-toto SLSA provenance statement.
    /// When omitted (or `enabled: false`), the attestation stage is a no-op.
    pub attestations: Option<AttestationConfig>,
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
    /// Monorepo configuration.
    /// When configured, tag discovery filters by tag_prefix and the working
    /// directory is scoped to dir.
    pub monorepo: Option<MonorepoConfig>,
    /// Makeself self-extracting archive configurations.
    #[serde(default, deserialize_with = "deserialize_makeselfs")]
    #[schemars(schema_with = "makeselfs_schema")]
    pub makeselfs: Vec<MakeselfConfig>,
    /// AppImage configurations. Each entry bundles a built Linux binary plus
    /// its desktop integration into a single self-contained `.AppImage` via
    /// linuxdeploy.
    #[serde(default, deserialize_with = "deserialize_appimages")]
    #[schemars(schema_with = "appimages_schema")]
    pub appimages: Vec<AppImageConfig>,
    /// Opt-in post-release verification gate. Runs LAST (after the release is
    /// created and every publisher has run) and REPORTS post-publish defects —
    /// missing assets, failed install smoke-tests, glibc-ceiling violations.
    /// Because it runs after the irreversible publish, a failure exits
    /// non-zero to flag CI but never undoes the release. Off unless
    /// `verify_release.enabled: true`.
    #[serde(default)]
    pub verify_release: VerifyReleaseConfig,
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
    /// the project-level retry policy).
    pub retry: Option<RetryConfig>,
    /// MCP (Model Context Protocol) server registry publishing
    /// configuration. When `name` is empty (the default), the publisher is
    /// skipped. The `mcp:` publisher block.
    #[serde(default)]
    pub mcp: McpConfig,
    /// SchemaStore publisher. Registers the project's JSON Schema(s) on
    /// SchemaStore at release time. When `schemas` is empty (the default),
    /// the publisher is skipped. The `schemastore:` publisher block.
    #[serde(default)]
    pub schemastore: crate::config::publishers::SchemastoreConfig,
    /// NPM package registry publishing configurations. One entry per
    /// published package. In the default `optional-deps` mode anodizer emits
    /// npm's native per-platform packages (biome / git-cliff pattern); in
    /// `postinstall` mode it emits a download shim (the `npms:`
    /// parity).
    pub npms: Option<Vec<NpmConfig>>,
    /// GemFury (fury.io) deb/rpm/apk publishing configurations. Mirrors
    /// The `gemfury:` block. The legacy spelling
    /// `furies:` is accepted via serde alias; a one-time deprecation
    /// warning is emitted by [`warn_on_legacy_furies_alias`].
    #[serde(alias = "furies")]
    pub gemfury: Option<Vec<GemFuryConfig>>,
    /// Per-crate metadata derived from each crate's `Cargo.toml [package]`
    /// table (description / license / homepage / authors). Populated at
    /// config-load time by [`Config::populate_derived_metadata`], keyed by
    /// crate name. NOT a user-facing YAML field — it backs the
    /// crate-aware `meta_*_for` accessors so a plain Rust project gets its
    /// publisher metadata without repeating it in a top-level `metadata:`
    /// block. A hand-written `metadata:` field and per-publisher overrides
    /// still win.
    #[serde(skip)]
    #[schemars(skip)]
    pub derived_metadata: BTreeMap<String, MetadataConfig>,
}

/// Helper schema function for the signs field (accepts object or array).
fn signs_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<SignConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "Artifact signing configurations (cosign, GPG, etc.). Accepts a single object or array."
            .into(),
    );
    schema
}

/// Helper schema function for the upx field (accepts object or array).
fn upx_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<UpxConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "UPX binary compression configurations. Accepts a single object or array.".into(),
    );
    schema
}

/// Helper schema function for the sboms field (accepts object or array).
fn sboms_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Vec<SbomConfig>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "SBOM generation configurations. Accepts a single object or array.".into(),
    );
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
            version_files: None,
            tag: None,
            git: None,
            partial: None,
            workspaces: None,
            source: None,
            sboms: Vec::new(),
            attestations: None,
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
            appimages: Vec::new(),
            verify_release: VerifyReleaseConfig::default(),
            srpms: None,
            milestones: None,
            uploads: None,
            aur_sources: None,
            retry: None,
            mcp: McpConfig::default(),
            schemastore: crate::config::publishers::SchemastoreConfig::default(),
            npms: None,
            gemfury: None,
            derived_metadata: BTreeMap::new(),
        }
    }
}

impl Config {
    /// The full crate universe: top-level `crates` plus every
    /// `workspaces[].crates` entry, deduplicated by name (first-seen wins,
    /// so a top-level entry shadows a same-named workspace entry).
    ///
    /// Single source of the read-only "all crates that can carry per-crate
    /// config" walk. Publisher registration, required/retain gate
    /// collapsing, per-crate dispatch, requirement derivation,
    /// `--crate`/`--all` selection, tool-need detection, artifact guards,
    /// and default-naming decisions must all resolve through this walker so
    /// a workspace-only crate carrying a publisher block is either visible
    /// everywhere or nowhere — a consumer iterating `config.crates`
    /// directly silently excludes workspace crates and hides their
    /// publishes. Only two shapes may keep a raw chained walk: mutation
    /// passes (`&mut` access — this walker hands out shared borrows) and
    /// validation/diagnostics that must see every entry as written,
    /// including the shadowed duplicates this walker dedups away.
    pub fn crate_universe(&self) -> Vec<&CrateConfig> {
        self.crate_universe_walk().0
    }

    /// Borrow a crate by name from [`Self::crate_universe`] (top-level wins
    /// on a name collision). The single by-name lookup every consumer must
    /// use — a `config.crates.iter().find(...)` cannot see workspace-only
    /// crates.
    pub fn find_crate(&self, name: &str) -> Option<&CrateConfig> {
        self.crate_universe().into_iter().find(|c| c.name == name)
    }

    /// Operator-facing warnings for crate-name collisions in the universe
    /// where the colliding entries disagree on `path` — almost certainly a
    /// config mistake (two distinct crates sharing a name). The legitimate
    /// duplicate (the same crate referenced from both top-level and a
    /// workspace) dedups silently. Emitted by the publish stage at entry so
    /// the warning appears once per run rather than once per universe walk.
    pub fn crate_universe_collision_warnings(&self) -> Vec<String> {
        self.crate_universe_walk().1
    }

    /// The one walk both [`Self::crate_universe`] and
    /// [`Self::crate_universe_collision_warnings`] derive from, so the
    /// merge/dedup policy and its diagnostics cannot diverge.
    fn crate_universe_walk(&self) -> (Vec<&CrateConfig>, Vec<String>) {
        let mut out: Vec<&CrateConfig> = self.crates.iter().collect();
        let mut warnings = Vec::new();
        for ws in self.workspaces.iter().flatten() {
            for c in &ws.crates {
                if let Some(existing) = out.iter().find(|e| e.name == c.name) {
                    if existing.path != c.path {
                        warnings.push(format!(
                            "workspace '{}' crate '{}' path '{}' shadowed by \
                             prior entry with path '{}'; workspace entry dropped (name \
                             collision with different paths — likely a config mistake)",
                            ws.name, c.name, c.path, existing.path
                        ));
                    }
                    continue;
                }
                out.push(c);
            }
        }
        (out, warnings)
    }

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

    /// The build targets compiled when neither a per-build `targets` nor
    /// `defaults.targets` is set: `defaults.targets` (when non-empty), else the
    /// canonical `DEFAULT_TARGETS`. Single source of truth for the target-set
    /// fallback — every target enumeration MUST resolve through this rather than
    /// re-deriving the fallback, so they never diverge.
    pub fn effective_default_targets(&self) -> Vec<String> {
        self.defaults
            .as_ref()
            .and_then(|d| d.targets.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                crate::target::DEFAULT_TARGETS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            })
    }

    /// The cross-compilation strategy applied to a crate that does not set its
    /// own `cross:` — `defaults.cross`, else `Auto`. SSOT for the per-crate
    /// strategy fallback.
    pub fn default_cross_strategy(&self) -> CrossStrategy {
        self.defaults
            .as_ref()
            .and_then(|d| d.cross.clone())
            .unwrap_or(CrossStrategy::Auto)
    }

    // --- Project metadata defaulting helpers ---
    //
    // Publishers that expose homepage/license/description/maintainer fields
    // fall back to these when their own field is unset, so a project only
    // needs to declare metadata once. Resolution precedence (highest first):
    //
    //   1. the per-publisher override (the publisher's own config field)
    //   2. a hand-written top-level `metadata:` YAML field
    //   3. the value derived from the crate's `Cargo.toml [package]` table
    //      (populated by `populate_derived_metadata`)
    //
    // Steps 1 is enforced by the publisher's `or_else(|| cfg.meta_*_for(..))`
    // chain; steps 2-3 are enforced inside the `meta_*_for` accessors. A
    // publisher that knows which crate it is publishing for should call the
    // crate-aware `meta_*_for(crate_name)` variant so workspace/per-crate
    // configs resolve each crate's OWN Cargo.toml metadata. The crate-agnostic
    // `meta_*` variants resolve the top-level `metadata:` block only (no
    // Cargo.toml fallback) and exist for truly project-level callers.

    /// Per-crate derived metadata for `crate_name`, if `Cargo.toml` supplied any.
    fn derived_for(&self, crate_name: &str) -> Option<&MetadataConfig> {
        self.derived_metadata.get(crate_name)
    }

    /// Name of the primary crate (first declared `crates:` entry, else the
    /// first workspace crate). Used as the metadata-derivation source and
    /// crate-name fallback for project-level publishers (e.g. top-level
    /// `homebrew_casks:`, `npms:`) that are not bound to a single crate.
    pub fn primary_crate_name(&self) -> Option<&str> {
        self.crate_universe().first().map(|c| c.name.as_str())
    }

    /// Project homepage: top-level `metadata.homepage` wins, else the primary
    /// crate's `Cargo.toml`-derived homepage. For project-level publishers
    /// (top-level casks) with no owning crate.
    pub fn meta_homepage_project(&self) -> Option<&str> {
        self.meta_homepage()
            .or_else(|| self.meta_homepage_for(self.primary_crate_name()?))
    }

    /// Project description: top-level `metadata.description` wins, else the
    /// primary crate's `Cargo.toml`-derived description.
    pub fn meta_description_project(&self) -> Option<&str> {
        self.meta_description()
            .or_else(|| self.meta_description_for(self.primary_crate_name()?))
    }

    /// Project source-repository URL: top-level `metadata.repository` wins, else
    /// the primary crate's `Cargo.toml`-derived repository. Backs the
    /// `{{ Metadata.Repository }}` template var.
    pub fn meta_repository_project(&self) -> Option<&str> {
        self.meta_repository()
            .or_else(|| self.meta_repository_for(self.primary_crate_name()?))
    }

    /// Project license: top-level `metadata.license` wins, else the primary
    /// crate's `Cargo.toml`-derived license. For the `{{ Metadata.License }}`
    /// template var and project-level publishers with no owning crate.
    pub fn meta_license_project(&self) -> Option<&str> {
        self.meta_license()
            .or_else(|| self.meta_license_for(self.primary_crate_name()?))
    }

    /// Project documentation URL: top-level `metadata.documentation` wins, else
    /// the primary crate's `Cargo.toml`-derived documentation URL.
    pub fn meta_documentation_project(&self) -> Option<&str> {
        self.meta_documentation()
            .or_else(|| self.meta_documentation_for(self.primary_crate_name()?))
    }

    /// Project homepage from `metadata.homepage` (top-level YAML only).
    pub fn meta_homepage(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.homepage.as_deref())
    }

    /// Project license from `metadata.license` (top-level YAML only).
    pub fn meta_license(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.license.as_deref())
    }

    /// Project source-repository URL from `metadata.repository` (top-level YAML only).
    pub fn meta_repository(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|m| m.repository.as_deref())
    }

    /// Project description from `metadata.description` (top-level YAML only).
    pub fn meta_description(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.description.as_deref())
    }

    /// Project documentation URL from `metadata.documentation` (top-level YAML only).
    pub fn meta_documentation(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|m| m.documentation.as_deref())
    }

    /// Project maintainers from `metadata.maintainers` (top-level YAML only).
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

    /// Homepage for `crate_name`: top-level `metadata.homepage` wins, else the
    /// value derived from the crate's `Cargo.toml [package]`.
    pub fn meta_homepage_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_homepage()
            .or_else(|| self.derived_for(crate_name)?.homepage.as_deref())
    }

    /// License for `crate_name`: top-level `metadata.license` wins, else the
    /// crate's `Cargo.toml [package].license` (never synthesised from
    /// `license-file`).
    pub fn meta_license_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_license()
            .or_else(|| self.derived_for(crate_name)?.license.as_deref())
    }

    /// Source-repository URL for `crate_name`: top-level `metadata.repository`
    /// wins, else the crate's `Cargo.toml [package].repository`. Feeds the npm
    /// `package.json` `repository` field so npm provenance validation (which
    /// matches it against the OIDC-claimed repository) passes without requiring
    /// the operator to restate the URL in the publisher config.
    pub fn meta_repository_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_repository()
            .or_else(|| self.derived_for(crate_name)?.repository.as_deref())
    }

    /// Description for `crate_name`: top-level `metadata.description` wins, else
    /// the crate's `Cargo.toml [package].description`.
    pub fn meta_description_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_description()
            .or_else(|| self.derived_for(crate_name)?.description.as_deref())
    }

    /// Documentation URL for `crate_name`: top-level `metadata.documentation`
    /// wins, else the crate's `Cargo.toml [package].documentation`.
    pub fn meta_documentation_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_documentation()
            .or_else(|| self.derived_for(crate_name)?.documentation.as_deref())
    }

    /// Maintainers for `crate_name`: top-level `metadata.maintainers` wins
    /// (when non-empty), else the crate's `Cargo.toml [package].authors`.
    pub fn meta_maintainers_for(&self, crate_name: &str) -> &[String] {
        let top = self.meta_maintainers();
        if !top.is_empty() {
            return top;
        }
        self.derived_for(crate_name)
            .and_then(|m| m.maintainers.as_deref())
            .unwrap_or(&[])
    }

    /// First maintainer for `crate_name` as "Name <email>" or just "Name".
    pub fn meta_first_maintainer_for(&self, crate_name: &str) -> Option<&str> {
        self.meta_maintainers_for(crate_name)
            .first()
            .map(|s| s.as_str())
    }

    /// Vendor / distributing-entity name for `crate_name`: the first
    /// maintainer with any `<email>` suffix stripped (e.g.
    /// `"Ada Lovelace <ada@x>"` → `"Ada Lovelace"`). `None` when no maintainer
    /// is derivable or the result is empty, so a Vendor field is never emitted
    /// blank. Reused by the rpm/deb Vendor and the OCI image `vendor` label.
    pub fn meta_vendor_for(&self, crate_name: &str) -> Option<String> {
        self.meta_first_maintainer_for(crate_name)
            .and_then(maintainer_name_only)
    }

    /// Populate [`Config::derived_metadata`] by reading each crate's
    /// `Cargo.toml [package]` table (description / license / homepage /
    /// authors), so publishers resolve a plain Rust project's metadata without
    /// requiring a top-level `metadata:` YAML block.
    ///
    /// Covers every crate the config knows about: top-level `crates:` plus
    /// every `workspaces[].crates[]`, so single-crate, workspace-lockstep, and
    /// per-crate configs all populate. Each crate is read from
    /// `<crate.path>/Cargo.toml` relative to `base_dir` (the directory the
    /// config was loaded from / the monorepo working directory).
    ///
    /// Idempotent and non-destructive: only fills entries; existing
    /// `derived_metadata` keys are overwritten with a fresh read. Crates whose
    /// `Cargo.toml` is missing or supplies nothing contribute an all-`None`
    /// entry (harmless — the accessors treat it as "no value").
    pub fn populate_derived_metadata(&mut self, base_dir: &std::path::Path) {
        let crate_paths: Vec<(String, String)> = self
            .crate_universe()
            .into_iter()
            .map(|c| (c.name.clone(), c.path.clone()))
            .collect();
        for (name, path) in crate_paths {
            let crate_dir = base_dir.join(&path);
            let derived = derive_metadata_from_cargo_toml(&crate_dir);
            self.derived_metadata.insert(name, derived);
        }
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

/// JSON Schema for the [`Config`] document as a canonical `serde_json::Value`,
/// in the JSON Schema draft-07 dialect.
///
/// The published `schema.json`, the `anodizer jsonschema` command, and the
/// config-reference doc generator all read the schema from this one function so
/// the dialect (`definitions` + `#/definitions/` refs) and the byte-form are
/// fixed in a single place. draft-07 is the dialect editors (VS Code, the JSON
/// Schema Store) resolve for `.anodizer.yaml`, so the published schema and the
/// editor integration agree.
///
/// Returns a plain `Value` rather than [`schemars::Schema`] deliberately:
/// serializing a `Schema` re-imposes schemars 1.x's keyword ordering (via its
/// internal `OrderedKeywordWrapper`), which would undo [`canonicalize_schema`].
/// Serializing the `Value` directly preserves the canonical order.
#[must_use]
pub fn config_schema() -> serde_json::Value {
    let schema = schemars::generate::SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Config>();
    let mut value = schema.to_value();
    canonicalize_schema(&mut value);
    value
}

/// JSON Schema keyword serialization order matching schemars 0.8's `SchemaObject`
/// field declaration order (its flattened `Metadata` / `SubschemaValidation` /
/// number / string / array / object validation structs concatenated in struct
/// order). The published `schema.json` is byte-pinned to this order so it stays
/// stable across schemars upgrades (1.x emits a different keyword order, and the
/// workspace builds `serde_json` with `preserve_order` — via `stage-publish` —
/// so insertion order leaks into the file unless re-imposed here). An unlisted
/// keyword sorts after all listed ones, then lexicographically.
const SCHEMA_KEYWORD_ORDER: &[&str] = &[
    "$id",
    "$schema",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "type",
    "format",
    "enum",
    "const",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "items",
    "additionalItems",
    "maxItems",
    "minItems",
    "uniqueItems",
    "contains",
    "maxProperties",
    "minProperties",
    "required",
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "$ref",
    "definitions",
];

/// Schema object keys whose VALUE is a map of name → subschema (not a subschema
/// itself). Their entries are sorted by NAME (schemars 0.8 backed these with a
/// `BTreeMap`); every other keyword's value is a schema whose own keys are
/// ordered by [`SCHEMA_KEYWORD_ORDER`].
const SCHEMA_DEFINITION_MAPS: &[&str] = &["properties", "patternProperties", "definitions"];

/// Re-impose schemars 0.8's deterministic serialization on a draft-07 schema
/// `Value` so the published artifact is byte-stable across schemars versions:
/// recursively (1) order each schema object's keys by [`SCHEMA_KEYWORD_ORDER`],
/// (2) sort definition-map entries (`properties`/`definitions`/…) by name,
/// (3) sort `required` (a set), and (4) normalize every `description` to single
/// spaces within a paragraph while preserving blank-line paragraph breaks.
fn canonicalize_schema(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            if let Some(Value::String(d)) = map.get_mut("description") {
                *d = collapse_description(d);
            }
            if let Some(Value::Array(required)) = map.get_mut("required") {
                required.sort_by(|a, b| match (a.as_str(), b.as_str()) {
                    (Some(x), Some(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            }
            // Recurse, treating each value by its role:
            // - a definition-map value (`properties`/`definitions`/…) is a
            //   name→schema map: sort its entries by name, recurse each schema;
            // - `default`/`enum`/`const`/`examples` hold literal instance DATA,
            //   not schemas — never reorder their keys (they preserve the config
            //   struct's serialization order);
            // - every other value is itself a schema (or array of schemas).
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    k if SCHEMA_DEFINITION_MAPS.contains(&k) => {
                        if let Value::Object(entries) = child {
                            sort_object_by_key(entries);
                            for sub in entries.values_mut() {
                                canonicalize_schema(sub);
                            }
                        }
                    }
                    "default" | "enum" | "const" | "examples" => {}
                    _ => canonicalize_schema(child),
                }
            }
            reorder_object(map, SCHEMA_KEYWORD_ORDER);
        }
        Value::Array(items) => {
            for item in items {
                canonicalize_schema(item);
            }
        }
        _ => {}
    }
}

/// Reorder `map`'s entries so listed keys come first in `order`, then any
/// remaining keys lexicographically. `serde_json`'s `preserve_order` feature is
/// active workspace-wide, so a `Map` serializes in insertion order — rebuilding
/// it in the target order fixes the serialized key order.
fn reorder_object(map: &mut serde_json::Map<String, serde_json::Value>, order: &[&str]) {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort_by(|a, b| {
        let rank = |k: &str| order.iter().position(|o| *o == k).unwrap_or(order.len());
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });
    let mut rebuilt = serde_json::Map::with_capacity(map.len());
    for k in keys {
        if let Some(v) = map.remove(&k) {
            rebuilt.insert(k, v);
        }
    }
    *map = rebuilt;
}

/// Sort an object map's entries by key (rebuilt because `preserve_order` keeps
/// insertion order). Used for definition maps where 0.8 emitted `BTreeMap`-sorted
/// names.
fn sort_object_by_key(map: &mut serde_json::Map<String, serde_json::Value>) {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    let mut rebuilt = serde_json::Map::with_capacity(map.len());
    for k in keys {
        if let Some(v) = map.remove(&k) {
            rebuilt.insert(k, v);
        }
    }
    *map = rebuilt;
}

/// Normalize a schema `description`: collapse each paragraph's internal
/// whitespace (including the rustdoc doc-comment's hard line wraps, which
/// schemars 1.x preserves verbatim) to single spaces, while preserving
/// blank-line paragraph breaks (`\n\n`). Reproduces the single-spaced,
/// paragraph-separated form earlier schemars releases emitted, so the published
/// schema's tooltips render as clean prose in editors.
fn collapse_description(s: &str) -> String {
    s.split("\n\n")
        .map(|para| {
            para.split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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

/// Validate `partial.by` up front so a stale value is rejected at config-load
/// time regardless of which target-resolution path runs.
///
/// `partial.by` is read in two unrelated places: the host-detection branch of
/// [`crate::partial::resolve_partial_target`] (which already rejects unknown
/// values) and the split-matrix generator (which treats anything that is not
/// `"os"` as `"target"`). Those two readers disagree on an out-of-set value
/// like the pre-rename `"goos"`: one errors, the other silently mis-groups the
/// matrix. Centralising the check means a typo fails loudly once, before
/// either reader can diverge.
pub fn validate_partial(config: &Config) -> Result<(), String> {
    if let Some(ref partial) = config.partial
        && let Some(ref by) = partial.by
    {
        match by.as_str() {
            "os" | "target" => {}
            other => {
                return Err(format!(
                    "unsupported partial.by value: \"{}\". \
                     Accepted values: \"os\", \"target\".",
                    other
                ));
            }
        }
    }
    Ok(())
}

/// Known OS values accepted by `archives[].format_overrides[].os`.
/// The Go runtime's `runtime.GOOS` values the archive pipe
/// recognises; anything outside this set is almost always a typo
/// (e.g. a Rust target triple slice like `pc-windows-msvc`).
const KNOWN_OS: &[&str] = &[
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
/// backend. A multiple-releases error, which
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

/// Validate that `release.on_failure` is set only at the root.
///
/// The failure policy is one process-wide decision per run, resolved
/// from the top-level `release:` block alone. Crate-level `release:`
/// blocks share the `ReleaseConfig` struct, so the field parses there
/// — but it would never be read; rejecting the misplacement at config
/// load keeps a policy choice from being silently ignored.
pub fn validate_on_failure_root_only(config: &Config) -> Result<(), String> {
    // Deliberately raw (not `crate_universe()`): validation must flag every
    // entry as written, including a workspace entry the dedup would shadow —
    // a policy violation on a shadowed crate is still a config mistake.
    let mut offenders: Vec<&str> = config
        .crates
        .iter()
        .chain(
            config
                .workspaces
                .iter()
                .flatten()
                .flat_map(|ws| ws.crates.iter()),
        )
        .filter(|c| c.release.as_ref().is_some_and(|r| r.on_failure.is_some()))
        .map(|c| c.name.as_str())
        .collect();
    offenders.sort_unstable();
    offenders.dedup();
    if offenders.is_empty() {
        return Ok(());
    }
    Err(format!(
        "release.on_failure is a root-level policy and cannot be set per crate \
         (set on: {}). Move it to the top-level `release:` block.",
        offenders.join(", ")
    ))
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

/// Validate `archives[].format_overrides[].os` values reject unknown OSes.
/// Silently no-op-ing unknown overrides has burned users typing
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
                if !KNOWN_OS.contains(&over.os.as_str()) {
                    let archive_id = archive.id.as_deref().unwrap_or("default");
                    return Err(format!(
                        "{}: archives[{}] (id={}): format_overrides.os=\"{}\" is not a recognised OS. \
                         Accepted values: {}.",
                        location,
                        idx,
                        archive_id,
                        over.os,
                        KNOWN_OS.join(", ")
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

    // Top-level homebrew_casks list (not nested under publish:) — not a
    // publish axis, so it is scanned separately from the visitor.
    if let Some(ref casks) = config.homebrew_casks {
        for (i, cask) in casks.iter().enumerate() {
            check(&format!("homebrew_casks[{i}]"), cask)?;
        }
    }

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(cask) = publish.homebrew_cask() {
            check(&axis.homebrew_cask_location(), cask)?;
        }
        Ok(())
    })
}

/// Allowed `winget.upgrade_behavior` values, mirroring the winget installer
/// manifest schema (1.12.0) `UpgradeBehavior` enum. A value outside this set
/// renders an installer manifest the winget validator rejects at PR time —
/// catch it at config-validate instead.
pub const WINGET_UPGRADE_BEHAVIORS: [&str; 3] = ["install", "uninstallPrevious", "deny"];

/// Validate that every configured `winget.upgrade_behavior` is one of the
/// winget-recognized values ([`WINGET_UPGRADE_BEHAVIORS`]). Walks the per-crate,
/// per-workspace, and `defaults.publish` axes.
pub fn validate_winget_upgrade_behavior(config: &Config) -> Result<(), String> {
    let check = |location: &str, winget: &WingetConfig| -> Result<(), String> {
        if let Some(ref behavior) = winget.upgrade_behavior
            && !WINGET_UPGRADE_BEHAVIORS.contains(&behavior.as_str())
        {
            return Err(format!(
                "{location}: upgrade_behavior `{behavior}` is not a valid winget value. \
                 Use one of: {}.",
                WINGET_UPGRADE_BEHAVIORS.join(", ")
            ));
        }
        Ok(())
    };

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(winget) = publish.winget() {
            check(&axis.winget_location(), winget)?;
        }
        Ok(())
    })
}

/// Validate that every `winget.dependencies[].architectures` entry names a
/// recognized WinGet architecture ([`WINGET_ARCHITECTURES`]). Walks the
/// per-crate, per-workspace, and `defaults.publish` axes.
///
/// The per-installer dependency emitter matches a scope value against each
/// installer's WinGet architecture by exact, case-sensitive equality. A value
/// outside the canonical set ([`WINGET_ARCHITECTURES`]: `x64`, `arm64`, `x86`)
/// therefore matches
/// no installer, so the dependency would silently disappear from the generated
/// manifest. Reject it at config-validate instead of shipping a manifest that
/// quietly omits a declared dependency. An empty list (or absent
/// `architectures`) means "all installers" and is valid.
pub fn validate_winget_dependency_architectures(config: &Config) -> Result<(), String> {
    let check = |location: &str, winget: &WingetConfig| -> Result<(), String> {
        let Some(ref deps) = winget.dependencies else {
            return Ok(());
        };
        for (i, dep) in deps.iter().enumerate() {
            let Some(ref scopes) = dep.architectures else {
                continue;
            };
            for scope in scopes {
                if !WINGET_ARCHITECTURES.contains(&scope.as_str()) {
                    return Err(format!(
                        "{location}: dependencies[{i}].architectures contains `{scope}`, \
                         which is not a valid winget architecture. Use one of: {} \
                         (or leave architectures empty/unset to apply the dependency \
                         to every installer).",
                        WINGET_ARCHITECTURES.join(", ")
                    ));
                }
            }
        }
        Ok(())
    };

    try_for_each_crate_publish(config, |axis, publish| {
        if let Some(winget) = publish.winget() {
            check(&axis.winget_location(), winget)?;
        }
        Ok(())
    })
}

/// Validate that `archives[].id` and `universal_binaries[].id` are unique
/// within their respective lists.
///
/// The id-uniqueness validation for archives and universal binaries.
/// Two archive
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
    fn check_unique(
        location: &str,
        kind: &str,
        ids: impl IntoIterator<Item = (usize, Option<String>)>,
    ) -> Result<(), String> {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (idx, maybe_id) in ids {
            // Empty is stored as "default" for archives via Default-time
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
        )
    };
    let check_unibins = |location: &str, ubs: &[UniversalBinaryConfig]| -> Result<(), String> {
        check_unique(
            location,
            "universal_binaries",
            ubs.iter().enumerate().map(|(i, u)| (i, u.id.clone())),
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
/// operator staged elsewhere. The validation rules below follow the
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
                            "{location}: build[{idx}] has a `prebuilt:` block but `builder:` \
                             is not `prebuilt`; the block is ignored. Set `builder: prebuilt` \
                             or remove the block."
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
    for krate in config.crate_universe() {
        match crate_all_prebuilt(krate) {
            Some(true) => saw_any = true,
            Some(false) => return false,
            None => {}
        }
    }
    saw_any
}

/// Validate the depth of `changelog.groups[].groups`.
///
/// Subgroups are capped at ONE level
/// (`/customization/publish/changelog.md`: "There can only be one level of
/// subgroups"). Anodizer's renderer can technically handle deeper nesting
/// (capped at 6 to match Markdown's heading limit), but accepting deeper
/// configs silently is a footgun: a config that works in anodizer but is
/// rejected here breaks parity for users migrating in.
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

/// Validate every upload-destination `exclude:` glob across all config axes.
///
/// `exclude:` drops artifacts whose file name matches a glob (see
/// [`crate::artifact::passes_exclude_filter`]). An unparseable glob is treated
/// as non-matching at runtime so it never crashes a release — but a typo'd
/// glob that silently keeps an asset (or, worse, drops every asset) is a
/// foot-gun. Reject malformed globs here, at config-load, with a clear message
/// before they can take effect.
///
/// Covers every config position where `exclude:` is settable: per-crate
/// `release:` and `blobs:` (top-level crates AND `workspaces[].crates[]`), the
/// top-level `artifactories:`, `cloudsmiths:`, `gemfury:`, and `uploads:`
/// lists, and the top-level shared `release:` block.
pub fn validate_exclude_globs(config: &Config) -> Result<(), String> {
    fn check(location: &str, exclude: Option<&[String]>) -> Result<(), String> {
        let Some(globs) = exclude else {
            return Ok(());
        };
        for (idx, g) in globs.iter().enumerate() {
            if g.is_empty() {
                return Err(format!(
                    "{location}: exclude[{idx}] is empty; remove the entry or set a \
                     real glob (an empty pattern matches nothing and is a no-op)"
                ));
            }
            if let Err(e) = glob::Pattern::new(g) {
                return Err(format!(
                    "{location}: exclude[{idx}] = {g:?} is not a valid glob: {e}"
                ));
            }
        }
        Ok(())
    }

    let check_crate = |location: &str, krate: &CrateConfig| -> Result<(), String> {
        if let Some(ref release) = krate.release {
            check(&format!("{location}.release"), release.exclude.as_deref())?;
        }
        if let Some(ref blobs) = krate.blobs {
            for (i, b) in blobs.iter().enumerate() {
                check(&format!("{location}.blobs[{i}]"), b.exclude.as_deref())?;
            }
        }
        Ok(())
    };

    for krate in &config.crates {
        check_crate(&format!("crates[{}]", krate.name), krate)?;
    }
    if let Some(ref ws_list) = config.workspaces {
        for ws in ws_list {
            for krate in &ws.crates {
                check_crate(
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    krate,
                )?;
            }
        }
    }
    if let Some(ref list) = config.artifactories {
        for (i, a) in list.iter().enumerate() {
            check(&format!("artifactories[{i}]"), a.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.cloudsmiths {
        for (i, c) in list.iter().enumerate() {
            check(&format!("cloudsmiths[{i}]"), c.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.gemfury {
        for (i, g) in list.iter().enumerate() {
            check(&format!("gemfury[{i}]"), g.exclude.as_deref())?;
        }
    }
    if let Some(ref list) = config.uploads {
        for (i, u) in list.iter().enumerate() {
            check(&format!("uploads[{i}]"), u.exclude.as_deref())?;
        }
    }
    if let Some(ref release) = config.release {
        check("release", release.exclude.as_deref())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-crate publish visitor
// ---------------------------------------------------------------------------

/// Identifies which of the three publish-config axes a visited block came from.
///
/// The config-validation walkers each format their own location string from
/// this identity, so different walkers can keep their distinct location wording
/// (`crate '{name}'` vs `crates[{name}].publish.homebrew_cask`) while sharing a
/// single iteration order: crates, then workspaces, then defaults.
pub(crate) enum PublishAxis<'a> {
    /// A top-level `crates[].publish` block, carrying the crate name.
    Crate { name: &'a str },
    /// A `workspaces[].crates[].publish` block, carrying the workspace and
    /// crate names.
    Workspace {
        workspace: &'a str,
        crate_name: &'a str,
    },
    /// The `defaults.publish` block.
    Defaults,
}

impl PublishAxis<'_> {
    /// Location string in the bare publish-block wording shared by the
    /// submitter-required and legacy-Homebrew-Formula warnings:
    /// `crate '{name}'`, `workspaces[{ws}].crates[{krate}]`, or
    /// `defaults.publish`.
    pub(crate) fn location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => format!("crate '{name}'"),
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}]"),
            PublishAxis::Defaults => "defaults.publish".to_string(),
        }
    }

    /// Location string in the cask-block wording used by the legacy
    /// Homebrew-Cask singular fold: `crates[{name}].publish.homebrew_cask`,
    /// `workspaces[{ws}].crates[{krate}].publish.homebrew_cask`, or
    /// `defaults.publish.homebrew_cask`.
    pub(crate) fn homebrew_cask_location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => {
                format!("crates[{name}].publish.homebrew_cask")
            }
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}].publish.homebrew_cask"),
            PublishAxis::Defaults => "defaults.publish.homebrew_cask".to_string(),
        }
    }

    /// Location string in the winget-block wording:
    /// `crates[{name}].publish.winget`,
    /// `workspaces[{ws}].crates[{krate}].publish.winget`, or
    /// `defaults.publish.winget`.
    pub(crate) fn winget_location(&self) -> String {
        match self {
            PublishAxis::Crate { name } => format!("crates[{name}].publish.winget"),
            PublishAxis::Workspace {
                workspace,
                crate_name,
            } => format!("workspaces[{workspace}].crates[{crate_name}].publish.winget"),
            PublishAxis::Defaults => "defaults.publish.winget".to_string(),
        }
    }
}

/// Shared, immutable view over the publisher sub-configs that appear on both
/// [`PublishConfig`] (the `crates[].publish` axis) and [`PublishDefaults`] (the
/// `defaults.publish` axis). The two underlying structs are distinct types, so
/// this enum erases the difference for read-only walkers.
pub(crate) enum PublishRef<'a> {
    /// A per-crate `publish:` block.
    Crate(&'a PublishConfig),
    /// The `defaults.publish:` block.
    Defaults(&'a PublishDefaults),
}

impl PublishRef<'_> {
    pub(crate) fn homebrew(&self) -> Option<&HomebrewConfig> {
        match self {
            PublishRef::Crate(p) => p.homebrew.as_ref(),
            PublishRef::Defaults(p) => p.homebrew.as_ref(),
        }
    }

    pub(crate) fn chocolatey(&self) -> Option<&ChocolateyConfig> {
        match self {
            PublishRef::Crate(p) => p.chocolatey.as_ref(),
            PublishRef::Defaults(p) => p.chocolatey.as_ref(),
        }
    }

    pub(crate) fn winget(&self) -> Option<&WingetConfig> {
        match self {
            PublishRef::Crate(p) => p.winget.as_ref(),
            PublishRef::Defaults(p) => p.winget.as_ref(),
        }
    }

    pub(crate) fn aur_source(&self) -> Option<&AurSourceConfig> {
        match self {
            PublishRef::Crate(p) => p.aur_source.as_ref(),
            PublishRef::Defaults(p) => p.aur_source.as_ref(),
        }
    }

    pub(crate) fn homebrew_cask(&self) -> Option<&HomebrewCaskConfig> {
        match self {
            PublishRef::Crate(p) => p.homebrew_cask.as_ref(),
            PublishRef::Defaults(p) => p.homebrew_cask.as_ref(),
        }
    }
}

/// Shared, mutable view over the publisher sub-configs that appear on both
/// [`PublishConfig`] and [`PublishDefaults`]. The `_mut` companion to
/// [`PublishRef`], for walkers that fold or rewrite a publisher block in place.
pub(crate) enum PublishMut<'a> {
    /// A per-crate `publish:` block.
    Crate(&'a mut PublishConfig),
    /// The `defaults.publish:` block.
    Defaults(&'a mut PublishDefaults),
}

impl PublishMut<'_> {
    pub(crate) fn homebrew_cask_mut(&mut self) -> Option<&mut HomebrewCaskConfig> {
        match self {
            PublishMut::Crate(p) => p.homebrew_cask.as_mut(),
            PublishMut::Defaults(p) => p.homebrew_cask.as_mut(),
        }
    }
}

/// Visit every `publish:` block across all three config axes — `crates[]`,
/// `workspaces[].crates[]`, then `defaults` — in that fixed order, passing each
/// block's [`PublishAxis`] identity and a read-only [`PublishRef`] view to
/// `visit`. Axes with no `publish:` block are skipped.
pub(crate) fn for_each_crate_publish<F>(config: &Config, mut visit: F)
where
    F: FnMut(PublishAxis<'_>, PublishRef<'_>),
{
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishRef::Crate(publish),
            );
        }
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishRef::Crate(publish),
                    );
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishRef::Defaults(publish));
    }
}

/// Fallible companion to [`for_each_crate_publish`]: visits the same three axes
/// in the same fixed order, but short-circuits on the first `Err` the callback
/// returns, propagating it to the caller. For validators that early-exit on the
/// first offending block.
pub(crate) fn try_for_each_crate_publish<F, E>(config: &Config, mut visit: F) -> Result<(), E>
where
    F: FnMut(PublishAxis<'_>, PublishRef<'_>) -> Result<(), E>,
{
    for krate in &config.crates {
        if let Some(ref publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishRef::Crate(publish),
            )?;
        }
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                if let Some(ref publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishRef::Crate(publish),
                    )?;
                }
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishRef::Defaults(publish))?;
    }

    Ok(())
}

/// Mutable companion to [`for_each_crate_publish`]: visits the same three axes
/// in the same fixed order, passing a [`PublishMut`] view so the callback can
/// rewrite the publisher block in place.
pub(crate) fn for_each_crate_publish_mut<F>(config: &mut Config, mut visit: F)
where
    F: FnMut(PublishAxis<'_>, PublishMut<'_>),
{
    for krate in &mut config.crates {
        if let Some(ref mut publish) = krate.publish {
            visit(
                PublishAxis::Crate { name: &krate.name },
                PublishMut::Crate(publish),
            );
        }
    }

    if let Some(ref mut workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &mut ws.crates {
                if let Some(ref mut publish) = krate.publish {
                    visit(
                        PublishAxis::Workspace {
                            workspace: &ws.name,
                            crate_name: &krate.name,
                        },
                        PublishMut::Crate(publish),
                    );
                }
            }
        }
    }

    if let Some(ref mut defaults) = config.defaults
        && let Some(ref mut publish) = defaults.publish
    {
        visit(PublishAxis::Defaults, PublishMut::Defaults(publish));
    }
}

/// A submitter moderation-queue advisory paired with the dispatch publisher
/// identity that produced it. The CLI filters by [`SubmitterAdvisory::publisher`]
/// so an advisory for a publisher deselected by `--skip` / `--publishers`
/// (e.g. `chocolatey` under a `--publishers npm` run) is suppressed instead of
/// emitted as noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitterAdvisory {
    /// Dispatch publisher name, matching the string
    /// [`crate::context::Context::publisher_deselected`] tests: `chocolatey`,
    /// `winget`, or `upstream-aur` (the AUR-source publisher's dispatch name).
    /// The CLI keys its deselection predicate on this value.
    pub publisher: String,
    /// The verbose advisory line surfaced to the operator.
    pub message: String,
}

/// One advisory per publisher configured with `required: true` whose group is
/// Submitter (chocolatey, winget, aur_source), each tagged with its dispatch
/// publisher identity so the CLI can suppress advisories for deselected
/// publishers.
///
/// `required: true` on a submitter still fails the release when the submission
/// itself fails (it feeds `required_failures()`), but the external moderation
/// outcome resolves after the release run and cannot be gated on. The advisory
/// is non-fatal and clarifies which half of the semantics applies. Cargo is
/// excluded: its default is already `required: true` and the message would be
/// noise.
///
/// Covers all three publish axes — `crates[].publish`,
/// `workspaces[].crates[].publish`, and `defaults.publish` (via
/// [`for_each_crate_publish`]) — plus the top-level `aur_sources:` list.
///
/// Pure: this returns the advisories without emitting them. The CLI surfaces
/// them through `StageLogger::verbose` (the `--verbose`-gated register), so
/// they stay hidden at the default log level — see
/// `pipeline::load_config_logged`.
pub fn submitter_required_warnings(config: &Config) -> Vec<SubmitterAdvisory> {
    fn advisory(location: &str, name: &str, publisher: &str) -> SubmitterAdvisory {
        SubmitterAdvisory {
            publisher: publisher.to_string(),
            message: format!(
                "{location}: publisher '{name}' submits to an external moderation queue; \
                 `required: true` fails the release when the submission itself fails, \
                 but the eventual moderation outcome happens outside the release run \
                 and cannot be gated."
            ),
        }
    }

    let mut warnings = Vec::new();

    for_each_crate_publish(config, |axis, publish| {
        let loc = axis.location();
        if publish.chocolatey().and_then(|c| c.required) == Some(true) {
            warnings.push(advisory(&loc, "chocolatey", "chocolatey"));
        }
        if publish.winget().and_then(|w| w.required) == Some(true) {
            warnings.push(advisory(&loc, "winget", "winget"));
        }
        if publish.aur_source().and_then(|a| a.required) == Some(true) {
            // The AUR-source publisher dispatches under the name `upstream-aur`
            // (`AurSourcePublisher::PUBLISHER_NAME`); key the advisory on that so
            // the CLI's `publisher_deselected("upstream-aur")` filter matches.
            warnings.push(advisory(&loc, "aur_source", "upstream-aur"));
        }
    });

    // Top-level aur_sources list (not nested under publish:) — no crate axis,
    // distinguish via the index in the list so two top-level entries collide cleanly.
    if let Some(ref sources) = config.aur_sources {
        for (idx, src) in sources.iter().enumerate() {
            if src.required == Some(true) {
                let loc = format!("top-level aur_sources[{idx}]");
                warnings.push(advisory(&loc, "aur_source", "upstream-aur"));
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

/// Reject the legacy V1 `dockers:` block at config-load time with a
/// clear migration error.
///
/// anodizer is V2-only by design: it implements `dockers_v2:` and the
/// associated multi-arch buildx flow, but does not ship the V1
/// `dockers: -> dockerfile + image_templates` pipe. Without this check the
/// top-level `Config` struct's `deny_unknown_fields` would emit a generic
/// "unknown field `dockers`" message that doesn't tell the user how to
/// migrate. This explicit error names the field, points at `dockers_v2:`,
/// and references the rationale.
///
pub fn validate_no_docker_v1(raw_yaml: &serde_yaml_ng::Value) -> Result<(), String> {
    if raw_yaml.get("dockers").is_some() {
        return Err(
            "config: legacy GoReleaser `dockers:` block is not supported — anodizer ships \
             dockers_v2: only (multi-arch buildx flow). Port the config to `dockers_v2:` per \
             https://anodize.dev/docs/migration/docker.html."
                .to_string(),
        );
    }
    Ok(())
}

/// Emit a `tracing::warn!` for each `publish.homebrew:` (Homebrew Formula)
/// occurrence in the loaded config. The upstream deprecated the
/// Formula publisher in favour of `homebrew_casks:`; anodizer mirrors the
/// upstream deprecation so users following the change-log see the
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

    for_each_crate_publish(config, |axis, publish| {
        if publish.homebrew().is_some() {
            warnings.push(formula_warning(&axis.location()));
        }
    });

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
/// The `furies → gemfury` rename messaging.
pub fn warn_on_legacy_furies_alias(raw_yaml: &serde_yaml_ng::Value) {
    if raw_yaml.get("furies").is_some() {
        tracing::warn!(
            "DEPRECATION: the top-level `furies:` config key is deprecated since GoReleaser \
             Pro v2.14; rename it to `gemfury:`. Both spellings are accepted but the legacy \
             key will be removed in a future release."
        );
    }
}

/// Emit a one-time deprecation warning for each nfpm config object that uses
/// the legacy `builds:` key. Serde transparently folds `builds:` into `ids:`
/// via `#[serde(alias = "builds")]` on [`NfpmConfig::ids`], so this function
/// consults the raw YAML pre-parse value to detect the legacy spelling that the
/// typed parse would otherwise erase.
///
/// The deprecated `NFPM.Builds` field (use `ids` instead).
///
/// nfpm config objects appear under the key `nfpm` or `nfpms` (a single map or
/// a sequence of maps) at multiple nesting depths — top-level, under
/// `defaults:`, under each `crates[]` entry, and under each
/// `workspaces[].crates[]` entry. Rather than enumerate every path, this walks
/// the tree recursively and inspects a node as an nfpm config only when it is
/// the value of an `nfpm:`/`nfpms:` key, so an unrelated `builds:` key
/// elsewhere (e.g. archives) is not double-counted.
pub fn warn_on_legacy_nfpm_builds(raw_yaml: &serde_yaml_ng::Value) {
    fn warn_for_nfpm_value(value: &serde_yaml_ng::Value) {
        match value {
            serde_yaml_ng::Value::Mapping(_) => {
                if value.get("builds").is_some() {
                    tracing::warn!(
                        "DEPRECATION: nfpm `builds:` is deprecated; use `ids:` instead. \
                         Both spellings are accepted but the legacy key will be removed in \
                         a future release."
                    );
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for item in items {
                    warn_for_nfpm_value(item);
                }
            }
            _ => {}
        }
    }

    fn descend(value: &serde_yaml_ng::Value) {
        match value {
            serde_yaml_ng::Value::Mapping(map) => {
                for (key, child) in map {
                    if matches!(key.as_str(), Some("nfpm") | Some("nfpms")) {
                        warn_for_nfpm_value(child);
                    }
                    descend(child);
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for item in items {
                    descend(item);
                }
            }
            _ => {}
        }
    }

    descend(raw_yaml);
}

/// Emit a one-time deprecation warning for each block that carries the legacy
/// `disable:` spelling of the canonical `skip:` field. Many config blocks
/// (`release`, `changelog`, `snapcraft`, the docker / installer / packager
/// blocks, …) accept `disable:` via `#[serde(alias = "disable")]` for
/// back-compat with imported configs; serde folds the alias into
/// `skip` on parse, erasing which spelling the user wrote. This helper
/// consults the raw YAML pre-parse value so porting users get a migration
/// prompt pointing at the canonical `skip:`.
///
/// Detection is allow-listed by enclosing block key, NOT a blind tree walk,
/// because free-form string-keyed maps would otherwise produce false
/// positives:
///   * Free-form string-keyed maps (`variables`, `derived_metadata`,
///     `build_args`, `labels`, `annotations`, `env`, header maps, …) let a
///     user legitimately name a key `disable`. Matching only when the key's
///     immediate enclosing block is allow-listed skips those — the nearest
///     named ancestor of such a key is the map's own key (e.g. `build_args`),
///     never an allow-listed block.
///
/// Axis-agnostic: the enclosing block key is identical whether the block sits
/// at the top level, under `defaults.<block>`, under `crates[].<block>`, or
/// under `workspaces[].crates[].<block>`, so a single nearest-named-ancestor
/// rule covers every placement.
pub fn warn_on_legacy_disable_alias(raw_yaml: &serde_yaml_ng::Value) {
    for msg in legacy_disable_alias_warnings(raw_yaml) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns one warning string per offending `disable:` key,
/// each naming the YAML path to the key. Exposed for tests; production callers
/// use [`warn_on_legacy_disable_alias`].
pub(crate) fn legacy_disable_alias_warnings(raw_yaml: &serde_yaml_ng::Value) -> Vec<String> {
    // Block key names whose struct exposes `skip` with `#[serde(alias =
    // "disable")]`. Resolved from the field's serde key on its parent (see the
    // `alias = "disable"` sites in core). `makeselfs` (top-level) and
    // `makeselves` (defaults.) both map to MakeselfConfig, so both are listed;
    // `gemfury` and its legacy `furies` alias both map to GemFuryConfig.
    const ALLOWLIST: &[&str] = &[
        "mcp",
        "makeselfs",
        "makeselves",
        "appimages",
        "msis",
        "pkgs",
        "nsis",
        "dockerhub",
        "release",
        "dockers_v2",
        "docker_v2",
        "changelog",
        "snapcrafts",
        "npms",
        "gemfury",
        "furies",
        "publishers",
        "sboms",
        "aur",
        "aur_source",
        "aur_sources",
        "blobs",
        "docker_digest",
        "checksum",
        "flatpaks",
    ];

    fn disable_warning(path: &str) -> String {
        format!(
            "DEPRECATION: {path}: legacy `disable:` is deprecated; rename it to `skip:`. \
             Both spellings are accepted but the legacy key will be removed in a future release."
        )
    }

    // `enclosing_block`: the nearest named (non-list-index) ancestor key — the
    // block the `disable:` key belongs to. Only warn when it is allow-listed.
    fn descend(
        value: &serde_yaml_ng::Value,
        path: &str,
        enclosing_block: Option<&str>,
        warnings: &mut Vec<String>,
    ) {
        match value {
            serde_yaml_ng::Value::Mapping(map) => {
                for (key, child) in map {
                    let Some(key) = key.as_str() else { continue };
                    let child_path = if path.is_empty() {
                        key.to_string()
                    } else {
                        format!("{path}.{key}")
                    };
                    if key == "disable"
                        && enclosing_block.is_some_and(|block| ALLOWLIST.contains(&block))
                    {
                        warnings.push(disable_warning(&child_path));
                    }
                    descend(child, &child_path, Some(key), warnings);
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for (idx, item) in items.iter().enumerate() {
                    let item_path = format!("{path}[{idx}]");
                    // A list index is not a named ancestor: keep the enclosing
                    // block (the list's own key) so e.g. `snapcrafts[0].disable`
                    // still resolves to the `snapcrafts` block.
                    descend(item, &item_path, enclosing_block, warnings);
                }
            }
            _ => {}
        }
    }

    let mut warnings = Vec::new();
    descend(raw_yaml, "", None, &mut warnings);
    warnings
}

/// Reject the legacy nested `mcp.github:` block with a
/// clear migration error.
///
/// The registry metadata that used to live under
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
/// `retry:` field is the legacy shape (retry handling moved to
/// the top-level `retry:` block); the per-pipe value is still honored at
/// resolve-time (see `stage-docker::resolve_retry_params`) but a top-level
/// `retry:` is the canonical surface for retry policy. Warning fires once
/// per occurrence so users porting from older configs see a clear
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
        if let Some(ref v2) = krate.dockers_v2 {
            for (i, cfg) in v2.iter().enumerate() {
                if cfg.retry.is_some() {
                    warnings.push(pipe_warning(
                        &format!("{prefix}.dockers_v2[{i}]"),
                        "dockers_v2",
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
        && let Some(ref v2) = defaults.dockers_v2
        && v2.retry.is_some()
    {
        warnings.push(pipe_warning("defaults.dockers_v2", "dockers_v2"));
    }

    warnings
}

/// Fold the deprecated singular Homebrew Cask fields into their canonical
/// plural lists and emit a one-time deprecation warning per folded field:
///
/// - `binary: <name>` → [`HomebrewCaskConfig::binaries`] (the upstream
///   renamed `binary:` to `binaries:`).
/// - `manpage: <page>` → [`HomebrewCaskConfig::manpages`].
///
/// anodizer accepts both spellings so imported configs keep parsing.
/// The captured values are moved out of [`HomebrewCaskConfig::legacy_binary`]
/// and [`HomebrewCaskConfig::legacy_manpage`] so downstream code only ever
/// reads the canonical plural fields.
///
/// The two folds use different insertion order: a legacy
/// `binary` is **prepended** to `binaries` so any explicit `binaries:` ordering
/// is preserved at the tail, whereas a legacy `manpage` is **appended** to
/// `manpages` (the cask renderer does
/// `brew.Manpages = append(brew.Manpages, brew.Manpage)`).
///
/// The fold runs across every config mode — top-level `homebrew_casks`,
/// per-crate `publish.homebrew_cask`, `workspaces[].crates[].publish`, and
/// `defaults.publish`.
pub fn apply_homebrew_cask_legacy_singulars(config: &mut Config) {
    /// Fold both deprecated singular fields (`binary:` → `binaries`,
    /// `manpage:` → `manpages`) on one cask, returning a warning per folded
    /// field. The singular `binary` is prepended to `binaries` so an explicit
    /// `binaries[0]` ordering is preserved at the tail; the singular `manpage`
    /// is appended to `manpages`.
    fn fold_one(location: &str, cask: &mut HomebrewCaskConfig) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(legacy) = cask.legacy_binary.take() {
            let entry = HomebrewCaskBinary::Name(legacy.clone());
            match cask.binaries {
                Some(ref mut list) => list.insert(0, entry),
                None => cask.binaries = Some(vec![entry]),
            }
            warnings.push(format!(
                "DEPRECATION: {location}: singular `binary: {legacy}` is deprecated since \
                 GoReleaser v2.12.6; use the plural `binaries: [{legacy}]` form. The legacy \
                 value has been folded into binaries[0]."
            ));
        }
        if let Some(legacy) = cask.legacy_manpage.take() {
            match cask.manpages {
                Some(ref mut list) => list.push(legacy.clone()),
                None => cask.manpages = Some(vec![legacy.clone()]),
            }
            warnings.push(format!(
                "DEPRECATION: {location}: singular `manpage: {legacy}` is deprecated; \
                 use the plural `manpages: [{legacy}]` form. The legacy value has been \
                 folded into manpages."
            ));
        }
        warnings
    }

    let mut warnings = Vec::new();

    // Top-level homebrew_casks list (not nested under publish:) — not a
    // publish axis, so it is scanned separately from the visitor.
    if let Some(ref mut casks) = config.homebrew_casks {
        for (i, cask) in casks.iter_mut().enumerate() {
            warnings.extend(fold_one(&format!("homebrew_casks[{i}]"), cask));
        }
    }

    for_each_crate_publish_mut(config, |axis, mut publish| {
        if let Some(cask) = publish.homebrew_cask_mut() {
            warnings.extend(fold_one(&axis.homebrew_cask_location(), cask));
        }
    });

    for msg in warnings {
        tracing::warn!("{}", msg);
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

mod completions;
pub use completions::*;

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
// AttestationConfig
// ---------------------------------------------------------------------------

mod attestation;
pub use attestation::*;

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

pub use crate::signing::{AuthenticodeConfig, DockerSignConfig, SignConfig};

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

mod cargo_metadata;
pub use cargo_metadata::derive_metadata_from_cargo_toml;

/// Extract the name portion of a `"Name <email>"` maintainer/author string,
/// dropping any `<…>` email suffix. Returns `None` when the result is empty
/// (e.g. a bare-email `<ada@example.com>`), so a derived Vendor / OCI `vendor`
/// value is never emitted blank.
pub fn maintainer_name_only(maintainer: &str) -> Option<String> {
    let name = maintainer.split('<').next().unwrap_or(maintainer).trim();
    (!name.is_empty()).then(|| name.to_string())
}

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
// VerifyReleaseConfig (top-level `verify_release:` post-publish gate)
// ---------------------------------------------------------------------------

mod verify_release;
pub use verify_release::*;

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

pub use crate::packagers::{
    AppImageConfig, AppImageExtra, MakeselfConfig, MakeselfFile, RuntimeHarvest, SrpmConfig,
};
pub(crate) use crate::packagers::{
    appimages_schema, deserialize_appimages, deserialize_makeselfs, makeselfs_schema,
};

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

// ---------------------------------------------------------------------------
// NpmConfig (NPM package registry publisher)
// ---------------------------------------------------------------------------

mod npm;
pub use npm::*;

// ---------------------------------------------------------------------------
// GemFuryConfig (Gemfury / fury.io publisher)
// ---------------------------------------------------------------------------

mod gemfury;
pub use gemfury::*;

// ---------------------------------------------------------------------------
// Well-known config file discovery
// ---------------------------------------------------------------------------

mod discovery;
pub use discovery::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
