use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::git::GitInfo;
use crate::log::{StageLogger, Verbosity};
use crate::partial::PartialTarget;
use crate::scm::ScmTokenType;
use crate::template::TemplateVars;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;

/// Valid --skip values for the `release` command (matches GoReleaser).
pub const VALID_RELEASE_SKIPS: &[&str] = &[
    "publish",
    "announce",
    "sign",
    "validate",
    "sbom",
    "docker",
    "winget",
    "chocolatey",
    "snapcraft",
    "snapcraft-publish",
    "scoop",
    "homebrew",
    "nix",
    "aur",
    "nfpm",
    "makeself",
    "flatpak",
    "srpm",
    "before",
    "notarize",
    "archive",
    "source",
    "build",
    "changelog",
    "release",
    "checksum",
    "upx",
    "blob",
    "templatefiles",
    "dmg",
    "msi",
    "nsis",
    "pkg",
    "appbundle",
];

/// Valid --skip values for the `build` command.
pub const VALID_BUILD_SKIPS: &[&str] = &["pre-hooks", "post-hooks", "validate", "before"];

/// Validate that all skip values are in the allowed set.
///
/// Returns `Ok(())` if all values are valid, or `Err` with a descriptive
/// message listing the invalid value(s) and the full set of valid options.
pub fn validate_skip_values(skip: &[String], valid: &[&str]) -> Result<(), String> {
    let invalid: Vec<&str> = skip
        .iter()
        .map(|s| s.as_str())
        .filter(|s| !valid.contains(s))
        .collect();
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid --skip value(s): {}. Valid options: {}",
            invalid.join(", "),
            valid.join(", "),
        ))
    }
}

pub struct ContextOptions {
    pub snapshot: bool,
    pub nightly: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
    pub skip_stages: Vec<String>,
    pub selected_crates: Vec<String>,
    pub token: Option<String>,
    /// Maximum number of parallel build jobs (minimum 1).
    pub parallelism: usize,
    /// When set, build only for this single host target triple.
    pub single_target: Option<String>,
    /// Path to a custom release notes file (overrides changelog).
    pub release_notes_path: Option<PathBuf>,
    /// When true, abort immediately on first error during publishing.
    pub fail_fast: bool,
    /// Partial build target for split/merge mode. When set, the build stage
    /// filters targets to only those matching this partial target.
    pub partial_target: Option<PartialTarget>,
    /// When true, running with `--merge` flag (merging artifacts from split builds).
    pub merge: bool,
    /// Explicit project root directory. When set, stages use this instead of
    /// discovering the repo root via `git rev-parse --show-toplevel`.
    pub project_root: Option<PathBuf>,
    /// Strict mode: configured features that would silently skip become errors.
    pub strict: bool,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            snapshot: false,
            nightly: false,
            dry_run: false,
            quiet: false,
            verbose: false,
            debug: false,
            skip_stages: Vec::new(),
            selected_crates: Vec::new(),
            token: None,
            parallelism: 4,
            single_target: None,
            release_notes_path: None,
            fail_fast: false,
            partial_target: None,
            merge: false,
            project_root: None,
            strict: false,
        }
    }
}

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    /// Set by changelog stage when `use: github-native` is configured.
    /// The release stage reads this to set `generate_release_notes(true)` on the GitHub API.
    pub github_native_changelog: bool,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
    pub changelogs: HashMap<String, String>,
    /// The resolved SCM token type (GitHub, GitLab, or Gitea).
    pub token_type: ScmTokenType,
    /// GoReleaser parity: set to true when any deprecated config field is used.
    pub deprecated: bool,
    /// Tracks which deprecation notices have already been shown (dedup).
    notified_deprecations: std::collections::HashSet<String>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            github_native_changelog: false,
            template_vars: vars,
            git_info: None,
            changelogs: HashMap::new(),
            token_type: ScmTokenType::GitHub,
            deprecated: false,
            notified_deprecations: std::collections::HashSet::new(),
        }
    }

    /// Log a deprecation warning for a config property.
    /// Each property is only warned about once (GoReleaser parity: deprecate.go).
    pub fn deprecate(&mut self, property: &str, message: &str) {
        if self.notified_deprecations.contains(property) {
            return;
        }
        self.notified_deprecations.insert(property.to_string());
        self.deprecated = true;
        eprintln!(
            "DEPRECATED: {} — see https://anodize.dev/deprecations#{}",
            message,
            property.replace('.', "-").to_lowercase()
        );
    }

    pub fn template_vars(&self) -> &TemplateVars {
        &self.template_vars
    }

    pub fn template_vars_mut(&mut self) -> &mut TemplateVars {
        &mut self.template_vars
    }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    /// Check whether "validate" is in the skip list.
    pub fn skip_validate(&self) -> bool {
        self.should_skip("validate")
    }

    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub fn is_snapshot(&self) -> bool {
        self.options.snapshot
    }

    pub fn is_strict(&self) -> bool {
        self.options.strict
    }

    /// In strict mode, return an error. In normal mode, log a warning and continue.
    /// Use this for any situation where a configured feature silently skips.
    pub fn strict_guard(&self, log: &crate::log::StageLogger, msg: &str) -> anyhow::Result<()> {
        if self.options.strict {
            anyhow::bail!("{} (strict mode)", msg);
        }
        log.warn(msg);
        Ok(())
    }

    /// Defense-in-depth helper for upload-style stages.
    ///
    /// Returns `true` (after logging the skip) when the context is in snapshot
    /// mode. Stages that perform external uploads (registries, package indexes,
    /// object storage, snap store, …) call this at entry so they no-op even
    /// when invoked directly without the orchestration layer's auto-skip.
    /// Centralising the check keeps every publish stage consistent and avoids
    /// per-stage copy-paste.
    pub fn skip_in_snapshot(&self, log: &crate::log::StageLogger, stage: &str) -> bool {
        if self.is_snapshot() {
            log.status(&format!("{}: skipped (snapshot mode)", stage));
            true
        } else {
            false
        }
    }

    /// Render a template, failing in strict mode on error, or falling back to the raw string.
    pub fn render_template_strict(
        &self,
        template: &str,
        label: &str,
        log: &crate::log::StageLogger,
    ) -> anyhow::Result<String> {
        match self.render_template(template) {
            Ok(rendered) => Ok(rendered),
            Err(e) => {
                if self.options.strict {
                    anyhow::bail!("{}: failed to render template: {} (strict mode)", label, e);
                }
                log.warn(&format!("{}: failed to render template: {}", label, e));
                Ok(template.to_string())
            }
        }
    }

    pub fn is_nightly(&self) -> bool {
        self.options.nightly
    }

    /// Set the `ReleaseURL` template variable.
    ///
    /// Should be called after a GitHub release is created, with the URL of
    /// the created release (e.g. `https://github.com/owner/repo/releases/tag/v1.0.0`).
    pub fn set_release_url(&mut self, url: &str) {
        self.template_vars.set("ReleaseURL", url);
    }

    /// Return the current `Version` template variable, or an empty string if
    /// not yet populated.
    pub fn version(&self) -> String {
        self.template_vars
            .get("Version")
            .cloned()
            .unwrap_or_default()
    }

    /// Derive the verbosity level from context options.
    pub fn verbosity(&self) -> Verbosity {
        Verbosity::from_flags(self.options.quiet, self.options.verbose, self.options.debug)
    }

    /// Create a [`StageLogger`] for the given stage name.
    pub fn logger(&self, stage: &'static str) -> StageLogger {
        StageLogger::new(stage, self.verbosity())
    }

    /// Populate template variables from `self.git_info`.
    ///
    /// Must be called after `self.git_info` is set. Sets the following vars:
    /// - `Tag`, `Version`, `RawVersion` — tag and version strings
    /// - `Major`, `Minor`, `Patch` — semver components
    /// - `Prerelease` — prerelease suffix (or empty)
    /// - `BuildMetadata` — build metadata from semver tag (or empty)
    /// - `FullCommit`, `Commit` — full commit SHA (`Commit` is alias for `FullCommit`)
    /// - `ShortCommit` — abbreviated commit SHA
    /// - `Branch` — current git branch
    /// - `CommitDate` — ISO 8601 author date of HEAD commit
    /// - `CommitTimestamp` — unix timestamp of HEAD commit
    /// - `IsGitDirty` — "true"/"false"
    /// - `IsGitClean` — "true"/"false" (inverse of `IsGitDirty`)
    /// - `GitTreeState` — "clean"/"dirty"
    /// - `GitURL` — git remote URL
    /// - `Summary` — git describe summary
    /// - `TagSubject` — annotated tag subject or commit subject
    /// - `TagContents` — full annotated tag message or commit message
    /// - `TagBody` — tag message body or commit message body
    /// - `IsSnapshot` — from context options
    /// - `IsNightly` — from context options
    /// - `IsDraft` — "false" (stages may override to "true")
    /// - `IsSingleTarget` — "true"/"false" based on single_target option
    /// - `PreviousTag` — previous matching tag, stripped in monorepo mode (or empty)
    /// - `PrefixedTag` — full tag with monorepo prefix, or tag_prefix-prepended (Pro addition)
    /// - `PrefixedPreviousTag` — full previous tag with prefix (Pro addition)
    /// - `PrefixedSummary` — full summary with prefix (Pro addition)
    /// - `IsRelease` — "true" if not snapshot and not nightly (Pro addition)
    /// - `IsMerging` — "true" if running with --merge flag (Pro addition)
    ///
    /// **Stage-scoped variables** (NOT set here; set per-artifact during stage execution):
    /// - `Binary` — binary name, set by build stage per binary and archive stage per archive
    /// - `ArtifactName` — output artifact filename, set by archive stage after creating each archive
    /// - `ArtifactPath` — absolute path to artifact, set by archive stage after creating each archive
    /// - `ArtifactExt` — artifact file extension (e.g. `.tar.gz`, `.exe`), set alongside ArtifactName
    /// - `ArtifactID` — build config `id` field, set by build stage per build config
    /// - `Os` — target OS, set by archive/nfpm stages per target
    /// - `Arch` — target architecture, set by archive/nfpm stages per target
    /// - `Target` — full target triple (e.g. `x86_64-unknown-linux-gnu`), set alongside Os/Arch
    /// - `Checksums` — combined checksum file contents, set by checksum stage
    pub fn populate_git_vars(&mut self) {
        if let Some(ref info) = self.git_info {
            // RawVersion: just major.minor.patch, no prerelease or build metadata.
            let raw_version = format!(
                "{}.{}.{}",
                info.semver.major, info.semver.minor, info.semver.patch
            );

            // Version: clean semver derived from the parsed SemVer struct, not
            // from the tag string.  The old `tag.strip_prefix('v')` approach
            // broke for monorepo workspace tags like `core-v0.3.2` because it
            // only stripped a leading 'v', leaving `core-v0.3.2` intact.
            // Deriving from the struct handles all tag_template prefixes.
            let mut version = raw_version.clone();
            if let Some(ref pre) = info.semver.prerelease {
                version.push('-');
                version.push_str(pre);
            }
            if let Some(ref meta) = info.semver.build_metadata {
                version.push('+');
                version.push_str(meta);
            }

            self.template_vars.set("Tag", &info.tag);
            self.template_vars.set("Version", &version);
            self.template_vars.set("RawVersion", &raw_version);
            self.template_vars
                .set("Major", &info.semver.major.to_string());
            self.template_vars
                .set("Minor", &info.semver.minor.to_string());
            self.template_vars
                .set("Patch", &info.semver.patch.to_string());
            self.template_vars.set(
                "Prerelease",
                info.semver.prerelease.as_deref().unwrap_or(""),
            );
            self.template_vars.set(
                "BuildMetadata",
                info.semver.build_metadata.as_deref().unwrap_or(""),
            );
            self.template_vars.set("FullCommit", &info.commit);
            self.template_vars.set("Commit", &info.commit);
            self.template_vars.set("ShortCommit", &info.short_commit);
            self.template_vars.set("Branch", &info.branch);
            self.template_vars.set("CommitDate", &info.commit_date);
            self.template_vars
                .set("CommitTimestamp", &info.commit_timestamp);
            self.template_vars
                .set("IsGitDirty", if info.dirty { "true" } else { "false" });
            self.template_vars
                .set("IsGitClean", if info.dirty { "false" } else { "true" });
            self.template_vars
                .set("GitTreeState", if info.dirty { "dirty" } else { "clean" });
            self.template_vars.set("GitURL", &info.remote_url);
            self.template_vars.set("Summary", &info.summary);
            self.template_vars.set("TagSubject", &info.tag_subject);
            self.template_vars.set("TagContents", &info.tag_contents);
            self.template_vars.set("TagBody", &info.tag_body);
            self.template_vars
                .set("PreviousTag", info.previous_tag.as_deref().unwrap_or(""));
            self.template_vars
                .set("FirstCommit", info.first_commit.as_deref().unwrap_or(""));

            // Pro additions: PrefixedTag, PrefixedPreviousTag, PrefixedSummary
            //
            // When monorepo.tag_prefix is configured, the git tag already
            // contains the prefix (e.g. "subproject1/v1.2.3"). In this case:
            //   - Tag = prefix stripped (e.g. "v1.2.3")
            //   - PrefixedTag = full tag (e.g. "subproject1/v1.2.3")
            //   - PrefixedPreviousTag = full previous tag
            //
            // When monorepo is NOT configured, fall back to the original
            // behavior: prepend tag.tag_prefix to construct PrefixedTag.
            let monorepo_prefix = self.config.monorepo_tag_prefix();

            // monorepo.tag_prefix takes precedence over tag.tag_prefix for
            // PrefixedTag / PrefixedPreviousTag / PrefixedSummary behavior.
            // When monorepo is configured, info.tag and info.summary already
            // contain the prefix from git, so we strip for the base vars and
            // use the raw values for the Prefixed variants.
            if let Some(prefix) = monorepo_prefix {
                // Monorepo mode: the tag in git_info is the FULL prefixed tag.
                // PrefixedTag = full tag (already has prefix).
                self.template_vars.set("PrefixedTag", &info.tag);

                // Tag = prefix stripped. Override the Tag we set above.
                let stripped_tag = crate::git::strip_monorepo_prefix(&info.tag, prefix);
                self.template_vars.set("Tag", stripped_tag);

                // Version: derive from the stripped tag (overrides the initial
                // value set above from info.tag, which in monorepo mode still
                // contains the prefix).
                let version = stripped_tag
                    .strip_prefix('v')
                    .unwrap_or(stripped_tag)
                    .to_string();
                self.template_vars.set("Version", &version);

                // PrefixedPreviousTag = full previous tag (already has prefix).
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                self.template_vars.set("PrefixedPreviousTag", prev_tag);

                // PreviousTag = prefix stripped, consistent with Tag being stripped.
                let stripped_prev = crate::git::strip_monorepo_prefix(prev_tag, prefix);
                self.template_vars.set("PreviousTag", stripped_prev);

                // PrefixedSummary: info.summary from `git describe` already
                // includes the monorepo prefix (e.g. "subproject1/v1.2.3-0-gabc123d"),
                // so use it as-is for the prefixed variant.
                self.template_vars.set("PrefixedSummary", &info.summary);
                // Summary: strip the monorepo prefix for the base variant.
                let stripped_summary = crate::git::strip_monorepo_prefix(&info.summary, prefix);
                self.template_vars.set("Summary", stripped_summary);
            } else {
                // Non-monorepo: prepend tag.tag_prefix to construct PrefixedTag.
                let tag_prefix = self
                    .config
                    .tag
                    .as_ref()
                    .and_then(|t| t.tag_prefix.as_deref())
                    .unwrap_or("");
                self.template_vars
                    .set("PrefixedTag", &format!("{}{}", tag_prefix, info.tag));
                let prev_tag = info.previous_tag.as_deref().unwrap_or("");
                let prefixed_prev = if prev_tag.is_empty() {
                    String::new()
                } else {
                    format!("{}{}", tag_prefix, prev_tag)
                };
                self.template_vars
                    .set("PrefixedPreviousTag", &prefixed_prev);
                self.template_vars.set(
                    "PrefixedSummary",
                    &format!("{}{}", tag_prefix, info.summary),
                );
            }
        }

        self.template_vars.set(
            "IsSnapshot",
            if self.options.snapshot {
                "true"
            } else {
                "false"
            },
        );
        self.template_vars.set(
            "IsNightly",
            if self.options.nightly {
                "true"
            } else {
                "false"
            },
        );
        // Wire IsDraft from config (GoReleaser reads ctx.Config.Release.Draft).
        let is_draft = self
            .config
            .release
            .as_ref()
            .and_then(|r| r.draft)
            .unwrap_or(false);
        self.template_vars
            .set("IsDraft", if is_draft { "true" } else { "false" });
        self.template_vars.set(
            "IsSingleTarget",
            if self.options.single_target.is_some() {
                "true"
            } else {
                "false"
            },
        );

        // Pro addition: IsRelease — true if this is a regular release (not snapshot, not nightly).
        let is_release = !self.options.snapshot && !self.options.nightly;
        self.template_vars
            .set("IsRelease", if is_release { "true" } else { "false" });

        // Pro addition: IsMerging — true if running with --merge flag.
        self.template_vars.set(
            "IsMerging",
            if self.options.merge { "true" } else { "false" },
        );
    }

    /// Populate time-related template variables using the current UTC time.
    ///
    /// Sets:
    /// - `Date` — current UTC time as RFC 3339
    /// - `Timestamp` — current unix timestamp as string
    /// - `Now` — current UTC time as RFC 3339
    /// - `Year` — four-digit year (e.g. "2026")
    /// - `Month` — zero-padded month (e.g. "03")
    /// - `Day` — zero-padded day (e.g. "30")
    /// - `Hour` — zero-padded hour (e.g. "14")
    /// - `Minute` — zero-padded minute (e.g. "05")
    pub fn populate_time_vars(&mut self) {
        let now = Utc::now();
        self.template_vars.set("Date", &now.to_rfc3339());
        self.template_vars
            .set("Timestamp", &now.timestamp().to_string());
        self.template_vars.set("Now", &now.to_rfc3339());
        self.template_vars
            .set("Year", &now.format("%Y").to_string());
        self.template_vars
            .set("Month", &now.format("%m").to_string());
        self.template_vars.set("Day", &now.format("%d").to_string());
        self.template_vars
            .set("Hour", &now.format("%H").to_string());
        self.template_vars
            .set("Minute", &now.format("%M").to_string());
    }

    /// Populate runtime environment variables.
    ///
    /// Sets:
    /// - `RuntimeGoos` — host OS in Go-compatible naming (e.g. "linux", "darwin", "windows")
    /// - `RuntimeGoarch` — host architecture in Go-compatible naming (e.g. "amd64", "arm64")
    /// - `Runtime_Goos` / `Runtime_Goarch` — GoReleaser-compatible nested aliases
    pub fn populate_runtime_vars(&mut self) {
        let goos = map_os_to_goos(std::env::consts::OS);
        let goarch = map_arch_to_goarch(std::env::consts::ARCH);
        self.template_vars.set("RuntimeGoos", goos);
        self.template_vars.set("RuntimeGoarch", goarch);
        // GoReleaser uses Runtime.Goos / Runtime.Goarch — after preprocessing
        // the dot becomes an underscore-separated flat key. We expose both forms.
        self.template_vars.set("Runtime_Goos", goos);
        self.template_vars.set("Runtime_Goarch", goarch);
    }

    /// Populate the `ReleaseNotes` template variable from stored changelogs.
    ///
    /// Should be called after the changelog stage has run and populated
    /// `self.changelogs`. Uses the first crate (by config order) whose
    /// changelog is present, or an empty string if no changelogs exist.
    /// Config order is deterministic, unlike HashMap iteration order.
    pub fn populate_release_notes_var(&mut self) {
        // Look up changelogs in config-defined crate order for determinism.
        let notes = self
            .config
            .crates
            .iter()
            .find_map(|c| self.changelogs.get(&c.name))
            .cloned()
            .unwrap_or_default();
        self.template_vars.set("ReleaseNotes", &notes);
    }

    /// Refresh the `Artifacts` structured template variable from the current
    /// artifact registry. Should be called before rendering release body and
    /// announce templates so they can iterate over all artifacts.
    ///
    /// Each artifact is serialized as a map with keys: `name`, `path`, `target`,
    /// `kind`, `crate_name`, and `metadata`.
    ///
    /// **Known metadata keys** (populated by individual stages):
    /// - `format` — archive format (e.g. `"tar.gz"`, `"zip"`), set by archive stage
    /// - `extra_file` — `"true"` when artifact is an extra file, set by checksum stage
    /// - `extra_name_template` — name template override for extra files, set by checksum stage
    /// - `digest` — docker image digest (e.g. `sha256:abc123...`), set by docker stage
    /// - `id` — artifact ID from config, set by docker and build stages
    /// - `binary` — binary name, set by build stage
    pub fn refresh_artifacts_var(&mut self) {
        // CSV metadata keys we expose as JSON arrays for template iteration.
        // Storage remains HashMap<String,String> (flat); only the
        // template-exposed view is expanded. Matches GoReleaser's
        // ExtraBinaries / ExtraFiles list semantics.
        const CSV_LIST_KEYS: &[&str] = &["extra_binaries", "extra_files"];

        let artifacts_value: Vec<serde_json::Value> = self
            .artifacts
            .all()
            .iter()
            .map(|a| {
                // Rebuild metadata map converting known CSV keys into arrays.
                let mut metadata_map = serde_json::Map::with_capacity(a.metadata.len());
                for (k, v) in &a.metadata {
                    if CSV_LIST_KEYS.contains(&k.as_str()) {
                        let items: Vec<serde_json::Value> = if v.is_empty() {
                            Vec::new()
                        } else {
                            v.split(',')
                                .map(|s| serde_json::Value::String(s.to_string()))
                                .collect()
                        };
                        metadata_map.insert(k.clone(), serde_json::Value::Array(items));
                    } else {
                        metadata_map.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                }
                serde_json::json!({
                    "name": a.name,
                    "path": a.path.to_string_lossy(),
                    "target": a.target.as_deref().unwrap_or(""),
                    "kind": a.kind.as_str(),
                    "crate_name": a.crate_name,
                    "metadata": serde_json::Value::Object(metadata_map),
                })
            })
            .collect();
        // serde_json::Value and tera::Value are the same type under the hood,
        // so no conversion is needed — pass values directly.
        let tera_value = tera::Value::Array(artifacts_value);
        self.template_vars.set_structured("Artifacts", tera_value);
    }

    /// Populate the `Metadata` structured template variable from config.metadata.
    ///
    /// Exposes the project metadata block as a nested map with PascalCase keys
    /// matching GoReleaser's `.Metadata.*` namespace:
    /// `Description`, `Homepage`, `License`, `Maintainers`, `ModTimestamp`.
    /// Missing fields default to empty strings / empty arrays.
    pub fn populate_metadata_var(&mut self) {
        let meta = self.config.metadata.as_ref();
        let description = meta.and_then(|m| m.description.as_deref()).unwrap_or("");
        let homepage = meta.and_then(|m| m.homepage.as_deref()).unwrap_or("");
        let license = meta.and_then(|m| m.license.as_deref()).unwrap_or("");
        let maintainers: Vec<&str> = meta
            .and_then(|m| m.maintainers.as_ref())
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        let mod_timestamp = meta.and_then(|m| m.mod_timestamp.as_deref()).unwrap_or("");

        let meta_map = serde_json::json!({
            "Description": description,
            "Homepage": homepage,
            "License": license,
            "Maintainers": maintainers,
            "ModTimestamp": mod_timestamp,
        });
        // serde_json::Value and tera::Value are the same type, so pass directly.
        self.template_vars.set_structured("Metadata", meta_map);
    }
}

/// Map Rust's `std::env::consts::OS` to Go-compatible GOOS naming.
/// GoReleaser templates expect Go runtime names (e.g. "darwin" not "macos").
pub fn map_os_to_goos(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other, // linux, windows, freebsd, etc. already match
    }
}

/// Map Rust's `std::env::consts::ARCH` to Go-compatible GOARCH naming.
/// GoReleaser templates expect Go runtime names (e.g. "amd64" not "x86_64").
pub fn map_arch_to_goarch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        "mips" => "mips",
        "mips64" => "mips64",
        "riscv64" => "riscv64",
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::{GitInfo, SemVer};

    fn make_git_info(dirty: bool, prerelease: Option<&str>) -> GitInfo {
        let tag = match prerelease {
            Some(pre) => format!("v1.2.3-{pre}"),
            None => "v1.2.3".to_string(),
        };
        GitInfo {
            tag,
            commit: "abc123def456abc123def456abc123def456abc1".to_string(),
            short_commit: "abc123d".to_string(),
            branch: "main".to_string(),
            dirty,
            semver: SemVer {
                major: 1,
                minor: 2,
                patch: 3,
                prerelease: prerelease.map(|s| s.to_string()),
                build_metadata: None,
            },
            commit_date: "2026-03-25T10:30:00+00:00".to_string(),
            commit_timestamp: "1774463400".to_string(),
            previous_tag: Some("v1.2.2".to_string()),
            remote_url: "https://github.com/test/repo.git".to_string(),
            summary: "v1.2.3-0-gabc123d".to_string(),
            tag_subject: "Release v1.2.3".to_string(),
            tag_contents: "Release v1.2.3\n\nFull release notes here.".to_string(),
            tag_body: "Full release notes here.".to_string(),
            first_commit: None,
        }
    }

    #[test]
    fn test_context_template_vars() {
        let mut config = Config::default();
        config.project_name = "test-project".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"test-project".to_string())
        );
    }

    #[test]
    fn test_context_should_skip() {
        let config = Config::default();
        let opts = ContextOptions {
            skip_stages: vec!["publish".to_string(), "announce".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(config, opts);
        assert!(ctx.should_skip("publish"));
        assert!(ctx.should_skip("announce"));
        assert!(!ctx.should_skip("build"));
    }

    #[test]
    fn test_context_render_template() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        let result = ctx.render_template("{{ .ProjectName }}-release").unwrap();
        assert_eq!(result, "myapp-release");
    }

    #[test]
    fn test_populate_git_vars_sets_all_expected_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("Major"), Some(&"1".to_string()));
        assert_eq!(v.get("Minor"), Some(&"2".to_string()));
        assert_eq!(v.get("Patch"), Some(&"3".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"".to_string()));
        assert_eq!(
            v.get("FullCommit"),
            Some(&"abc123def456abc123def456abc123def456abc1".to_string())
        );
        assert_eq!(v.get("ShortCommit"), Some(&"abc123d".to_string()));
        assert_eq!(v.get("Branch"), Some(&"main".to_string()));
        assert_eq!(
            v.get("CommitDate"),
            Some(&"2026-03-25T10:30:00+00:00".to_string())
        );
        assert_eq!(v.get("CommitTimestamp"), Some(&"1774463400".to_string()));
        assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
    }

    #[test]
    fn test_commit_is_alias_for_full_commit() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Commit"), v.get("FullCommit"));
    }

    #[test]
    fn test_populate_git_vars_prerelease() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, Some("rc.1")));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Version"), Some(&"1.2.3-rc.1".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.2.3".to_string()));
        assert_eq!(v.get("Prerelease"), Some(&"rc.1".to_string()));
    }

    #[test]
    fn test_build_metadata_template_var() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "v1.2.3+build.42".to_string();
        info.semver.build_metadata = Some("build.42".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("BuildMetadata"), Some(&"build.42".to_string()));
        // Version should include build metadata (strip v prefix only)
        assert_eq!(v.get("Version"), Some(&"1.2.3+build.42".to_string()));
    }

    #[test]
    fn test_build_metadata_empty_when_none() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("BuildMetadata"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_git_vars_monorepo_prefixed_tag() {
        // Workspace tags like "core-v0.3.2" should produce Version="0.3.2",
        // not "core-v0.3.2" (which breaks RPM Version fields and templates).
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "core-v0.3.2".to_string();
        info.semver = SemVer {
            major: 0,
            minor: 3,
            patch: 2,
            prerelease: None,
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"core-v0.3.2".to_string()));
        assert_eq!(v.get("Version"), Some(&"0.3.2".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"0.3.2".to_string()));
        assert_eq!(v.get("Major"), Some(&"0".to_string()));
        assert_eq!(v.get("Minor"), Some(&"3".to_string()));
        assert_eq!(v.get("Patch"), Some(&"2".to_string()));
    }

    #[test]
    fn test_populate_git_vars_monorepo_prefixed_tag_with_prerelease() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.tag = "operator-v1.0.0-rc.1".to_string();
        info.semver = SemVer {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: Some("rc.1".to_string()),
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("Tag"), Some(&"operator-v1.0.0-rc.1".to_string()));
        assert_eq!(v.get("Version"), Some(&"1.0.0-rc.1".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"1.0.0".to_string()));
    }

    #[test]
    fn test_git_tree_state_clean() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("IsGitDirty"), Some(&"false".to_string()));
        assert_eq!(v.get("GitTreeState"), Some(&"clean".to_string()));
    }

    #[test]
    fn test_git_tree_state_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("IsGitDirty"), Some(&"true".to_string()));
        assert_eq!(v.get("GitTreeState"), Some(&"dirty".to_string()));
    }

    #[test]
    fn test_is_snapshot_reflects_context_options() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );

        // Non-snapshot
        let config2 = Config::default();
        let opts2 = ContextOptions {
            snapshot: false,
            ..Default::default()
        };
        let mut ctx2 = Context::new(config2, opts2);
        ctx2.git_info = Some(make_git_info(false, None));
        ctx2.populate_git_vars();

        assert_eq!(
            ctx2.template_vars().get("IsSnapshot"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_draft_defaults_to_false() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_previous_tag_empty_when_none() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PreviousTag"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_time_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_time_vars();

        let v = ctx.template_vars();

        // Date should be RFC 3339 format (e.g. 2026-03-30T12:00:00+00:00)
        let date = v.get("Date").expect("Date should be set");
        assert!(
            date.contains('T') && date.len() > 10,
            "Date should be RFC 3339, got: {date}"
        );

        // Timestamp should be numeric
        let ts = v.get("Timestamp").expect("Timestamp should be set");
        assert!(
            ts.parse::<i64>().is_ok(),
            "Timestamp should be a numeric string, got: {ts}"
        );

        // Now should be ISO 8601
        let now = v.get("Now").expect("Now should be set");
        assert!(now.contains('T'), "Now should be ISO 8601, got: {now}");
    }

    #[test]
    fn test_env_vars_accessible_in_templates() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set_env("MY_VAR", "hello-world");
        ctx.template_vars_mut().set_env("DEPLOY_ENV", "staging");

        let result = ctx
            .render_template("{{ .Env.MY_VAR }}-{{ .Env.DEPLOY_ENV }}")
            .unwrap();
        assert_eq!(result, "hello-world-staging");
    }

    #[test]
    fn test_populate_git_vars_without_git_info_still_sets_snapshot() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // Don't set git_info — populate_git_vars should still set IsSnapshot/IsDraft
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSnapshot"),
            Some(&"true".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("IsDraft"),
            Some(&"false".to_string())
        );
        // Git-specific vars should NOT be set
        assert_eq!(ctx.template_vars().get("Tag"), None);
    }

    #[test]
    fn test_is_nightly_set_when_nightly_mode_active() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
            "IsNightly should be 'true' when nightly mode is active"
        );
        assert!(ctx.is_nightly(), "is_nightly() should return true");
    }

    #[test]
    fn test_is_nightly_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"false".to_string()),
            "IsNightly should default to 'false'"
        );
        assert!(
            !ctx.is_nightly(),
            "is_nightly() should return false by default"
        );
    }

    #[test]
    fn test_version_returns_populated_value() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(ctx.version(), "1.2.3");
    }

    #[test]
    fn test_version_returns_empty_when_not_set() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(ctx.version(), "");
    }

    #[test]
    fn test_is_nightly_without_git_info() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        // No git_info set — populate_git_vars still sets IsNightly
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsNightly"),
            Some(&"true".to_string()),
            "IsNightly should be set even without git info"
        );
    }

    #[test]
    fn test_is_git_clean_when_not_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsGitClean"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_git_clean_when_dirty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(true, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsGitClean"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_git_url_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("GitURL"),
            Some(&"https://github.com/test/repo.git".to_string())
        );
    }

    #[test]
    fn test_summary_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("Summary"),
            Some(&"v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_tag_subject_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagSubject"),
            Some(&"Release v1.2.3".to_string())
        );
    }

    #[test]
    fn test_tag_contents_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagContents"),
            Some(&"Release v1.2.3\n\nFull release notes here.".to_string())
        );
    }

    #[test]
    fn test_tag_body_set_from_git_info() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("TagBody"),
            Some(&"Full release notes here.".to_string())
        );
    }

    #[test]
    fn test_is_single_target_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_single_target_true_when_set() {
        let config = Config::default();
        let opts = ContextOptions {
            single_target: Some("x86_64-unknown-linux-gnu".to_string()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsSingleTarget"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_populate_runtime_vars() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_runtime_vars();

        let v = ctx.template_vars();

        let goos = v.get("RuntimeGoos").expect("RuntimeGoos should be set");
        assert!(
            !goos.is_empty(),
            "RuntimeGoos should not be empty, got: {goos}"
        );
        // RuntimeGoos uses Go naming (e.g. "darwin" not "macos")
        assert_eq!(goos, map_os_to_goos(std::env::consts::OS));

        let goarch = v.get("RuntimeGoarch").expect("RuntimeGoarch should be set");
        assert!(
            !goarch.is_empty(),
            "RuntimeGoarch should not be empty, got: {goarch}"
        );
        // RuntimeGoarch uses Go naming (e.g. "amd64" not "x86_64")
        assert_eq!(goarch, map_arch_to_goarch(std::env::consts::ARCH));
    }

    #[test]
    fn test_populate_release_notes_var_with_changelogs() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "my-crate".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.changelogs
            .insert("my-crate".to_string(), "## Changes\n- fix bug".to_string());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"## Changes\n- fix bug".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_empty_when_no_changelogs() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_release_notes_var();

        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_populate_release_notes_var_deterministic_with_multiple_crates() {
        let mut config = Config::default();
        config.crates.push(crate::config::CrateConfig {
            name: "crate-a".to_string(),
            ..Default::default()
        });
        config.crates.push(crate::config::CrateConfig {
            name: "crate-b".to_string(),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.changelogs
            .insert("crate-a".to_string(), "notes-a".to_string());
        ctx.changelogs
            .insert("crate-b".to_string(), "notes-b".to_string());
        ctx.populate_release_notes_var();

        // Should always pick the first crate in config order, not arbitrary HashMap order
        assert_eq!(
            ctx.template_vars().get("ReleaseNotes"),
            Some(&"notes-a".to_string())
        );
    }

    #[test]
    fn test_outputs_accessible_in_templates() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set_output("build_id", "abc123");
        ctx.template_vars_mut()
            .set_output("deploy_url", "https://example.com");

        let result = ctx
            .render_template("{{ .Outputs.build_id }}-{{ .Outputs.deploy_url }}")
            .unwrap();
        assert_eq!(result, "abc123-https://example.com");
    }

    #[test]
    fn test_artifact_ext_and_target_template_vars() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactName", "myapp.tar.gz");
        ctx.template_vars_mut().set("ArtifactExt", ".tar.gz");
        ctx.template_vars_mut()
            .set("Target", "x86_64-unknown-linux-gnu");

        let result = ctx
            .render_template("{{ .ArtifactExt }}_{{ .Target }}")
            .unwrap();
        assert_eq!(result, ".tar.gz_x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_checksums_template_var() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        let checksum_text = "abc123  myapp.tar.gz\ndef456  myapp.zip\n";
        ctx.template_vars_mut().set("Checksums", checksum_text);

        let result = ctx.render_template("{{ .Checksums }}").unwrap();
        assert_eq!(result, checksum_text);
    }

    // --- Pro template variable tests ---

    #[test]
    fn test_prefixed_tag_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedTag"),
            Some(&"api/v1.2.3".to_string())
        );
    }

    #[test]
    fn test_prefixed_tag_without_tag_prefix() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        // No tag_prefix configured — PrefixedTag should equal Tag
        assert_eq!(
            ctx.template_vars().get("PrefixedTag"),
            Some(&"v1.2.3".to_string())
        );
    }

    #[test]
    fn test_prefixed_previous_tag_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedPreviousTag"),
            Some(&"api/v1.2.2".to_string())
        );
    }

    #[test]
    fn test_prefixed_previous_tag_empty_when_no_previous() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut info = make_git_info(false, None);
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        // When there is no previous tag, PrefixedPreviousTag should be empty
        // (not just the prefix), matching GoReleaser behavior.
        assert_eq!(
            ctx.template_vars().get("PrefixedPreviousTag"),
            Some(&"".to_string())
        );
    }

    #[test]
    fn test_prefixed_summary_with_tag_prefix() {
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("PrefixedSummary"),
            Some(&"api/v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_is_release_true_for_normal_release() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsRelease"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_release_false_for_snapshot() {
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsRelease"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_release_false_for_nightly() {
        let config = Config::default();
        let opts = ContextOptions {
            nightly: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsRelease"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_is_merging_true_when_merge_flag_set() {
        let config = Config::default();
        let opts = ContextOptions {
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsMerging"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_merging_false_by_default() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsMerging"),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn test_refresh_artifacts_var_empty() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.refresh_artifacts_var();

        // Should render as an empty array
        let result = ctx
            .render_template("{% for a in Artifacts %}{{ a.name }}{% endfor %}")
            .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_refresh_artifacts_var_with_artifacts() {
        use crate::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        // Artifacts are created with empty `name` — ArtifactRegistry::add()
        // auto-derives the name from the path's filename component when name
        // is empty (see artifact.rs add() implementation).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.refresh_artifacts_var();

        // Iterate over artifacts and collect names
        let result = ctx
            .render_template("{% for a in Artifacts %}{{ a.name }},{% endfor %}")
            .unwrap();
        assert!(result.contains("myapp-1.0.0-linux-amd64.tar.gz"));
        assert!(result.contains("myapp"));

        // Check kind field
        let result_kinds = ctx
            .render_template("{% for a in Artifacts %}{{ a.kind }},{% endfor %}")
            .unwrap();
        assert!(result_kinds.contains("archive"));
        assert!(result_kinds.contains("binary"));
    }

    #[test]
    fn test_populate_metadata_var_with_mod_timestamp() {
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            mod_timestamp: Some("{{ .CommitTimestamp }}".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var();

        // Metadata should be accessible as a nested map with PascalCase keys
        let result = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
        assert_eq!(result, "{{ .CommitTimestamp }}");
    }

    #[test]
    fn test_populate_metadata_var_empty_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var();

        // Should render empty strings for missing fields (PascalCase keys)
        let result = ctx.render_template("{{ Metadata.Description }}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_populate_metadata_var_reads_from_config() {
        let mut config = Config::default();
        config.metadata = Some(crate::config::MetadataConfig {
            description: Some("A test project".to_string()),
            homepage: Some("https://example.com".to_string()),
            license: Some("MIT".to_string()),
            maintainers: Some(vec!["Alice".to_string(), "Bob".to_string()]),
            mod_timestamp: Some("1234567890".to_string()),
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.populate_metadata_var();

        let desc = ctx.render_template("{{ Metadata.Description }}").unwrap();
        assert_eq!(desc, "A test project");

        let home = ctx.render_template("{{ Metadata.Homepage }}").unwrap();
        assert_eq!(home, "https://example.com");

        let lic = ctx.render_template("{{ Metadata.License }}").unwrap();
        assert_eq!(lic, "MIT");

        let ts = ctx.render_template("{{ Metadata.ModTimestamp }}").unwrap();
        assert_eq!(ts, "1234567890");
    }

    #[test]
    fn test_artifact_id_template_var() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactID", "default");

        let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
        assert_eq!(result, "default");
    }

    #[test]
    fn test_artifact_id_empty_when_not_set() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ArtifactID", "");

        let result = ctx.render_template("{{ .ArtifactID }}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_pro_vars_rendered_in_templates() {
        // Test that all Pro vars can be used in templates together
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("api/".to_string()),
            ..Default::default()
        });
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let result = ctx
            .render_template(
                "{% if IsRelease %}release{% endif %}-{% if IsMerging %}merge{% endif %}-{{ .PrefixedTag }}",
            )
            .unwrap();
        assert_eq!(result, "release-merge-api/v1.2.3");
    }

    #[test]
    fn test_is_release_without_git_info() {
        // IsRelease should still be set even without git info
        let config = Config::default();
        let opts = ContextOptions {
            snapshot: false,
            nightly: false,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsRelease"),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn test_is_merging_without_git_info() {
        // IsMerging should still be set even without git info
        let config = Config::default();
        let opts = ContextOptions {
            merge: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.populate_git_vars();

        assert_eq!(
            ctx.template_vars().get("IsMerging"),
            Some(&"true".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Monorepo template variable tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_monorepo_tag_prefix_strips_tag_for_template_var() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("subproject1/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        // Simulate a monorepo tag: the full prefixed tag is stored in git_info.
        let mut info = make_git_info(false, None);
        info.tag = "subproject1/v1.2.3".to_string();
        info.previous_tag = Some("subproject1/v1.2.2".to_string());
        info.summary = "subproject1/v1.2.3-0-gabc123d".to_string();
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Tag should have the prefix stripped.
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // Version should derive from stripped tag.
        assert_eq!(v.get("Version"), Some(&"1.2.3".to_string()));
        // PrefixedTag should retain the full tag.
        assert_eq!(
            v.get("PrefixedTag"),
            Some(&"subproject1/v1.2.3".to_string())
        );
        // PreviousTag should be stripped (consistent with Tag).
        assert_eq!(v.get("PreviousTag"), Some(&"v1.2.2".to_string()));
        // PrefixedPreviousTag should retain the full tag.
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"subproject1/v1.2.2".to_string())
        );
        // Summary should be stripped.
        assert_eq!(v.get("Summary"), Some(&"v1.2.3-0-gabc123d".to_string()));
        // PrefixedSummary should retain the full summary.
        assert_eq!(
            v.get("PrefixedSummary"),
            Some(&"subproject1/v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_monorepo_prefixed_previous_tag() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v2.0.0".to_string();
        info.previous_tag = Some("svc/v1.9.0".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // PrefixedPreviousTag should be the full previous tag.
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"svc/v1.9.0".to_string())
        );
        // PreviousTag should be stripped (prefix removed), consistent with Tag.
        assert_eq!(v.get("PreviousTag"), Some(&"v1.9.0".to_string()));
    }

    #[test]
    fn test_no_monorepo_falls_back_to_tag_prefix() {
        // When monorepo is not set, PrefixedTag should use tag.tag_prefix.
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("release/".to_string()),
            ..Default::default()
        });
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.git_info = Some(make_git_info(false, None));
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Tag is plain "v1.2.3" (not stripped because no monorepo).
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // PrefixedTag should prepend tag_prefix.
        assert_eq!(v.get("PrefixedTag"), Some(&"release/v1.2.3".to_string()));
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"release/v1.2.2".to_string())
        );
    }

    #[test]
    fn test_monorepo_overrides_tag_prefix_for_prefixed_vars() {
        // When both monorepo.tag_prefix and tag.tag_prefix are set,
        // monorepo should take precedence for PrefixedTag.
        let mut config = Config::default();
        config.tag = Some(crate::config::TagConfig {
            tag_prefix: Some("release/".to_string()),
            ..Default::default()
        });
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v1.2.3".to_string();
        info.previous_tag = Some("svc/v1.2.2".to_string());
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        // Monorepo takes precedence: Tag is stripped.
        assert_eq!(v.get("Tag"), Some(&"v1.2.3".to_string()));
        // PrefixedTag is the full monorepo tag, NOT tag_prefix-prepended.
        assert_eq!(v.get("PrefixedTag"), Some(&"svc/v1.2.3".to_string()));
    }

    #[test]
    fn test_monorepo_prefixed_summary() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("pkg/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "pkg/v1.2.3".to_string();
        // In a real monorepo, `git describe` already includes the prefix in the summary.
        info.summary = "pkg/v1.2.3-0-gabc123d".to_string();
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        // PrefixedSummary is info.summary as-is (already contains prefix).
        assert_eq!(
            ctx.template_vars().get("PrefixedSummary"),
            Some(&"pkg/v1.2.3-0-gabc123d".to_string())
        );
        // Summary should have the prefix stripped.
        assert_eq!(
            ctx.template_vars().get("Summary"),
            Some(&"v1.2.3-0-gabc123d".to_string())
        );
    }

    #[test]
    fn test_monorepo_no_previous_tag() {
        let mut config = Config::default();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("svc/".to_string()),
            dir: None,
        });
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut info = make_git_info(false, None);
        info.tag = "svc/v1.0.0".to_string();
        info.previous_tag = None;
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();
        assert_eq!(v.get("PrefixedPreviousTag"), Some(&"".to_string()));
        // PreviousTag should also be empty when no previous tag exists.
        assert_eq!(v.get("PreviousTag"), Some(&"".to_string()));
    }

    // -----------------------------------------------------------------------
    // Integration test: full monorepo flow
    // -----------------------------------------------------------------------

    #[test]
    fn test_monorepo_full_flow_all_vars() {
        // End-to-end test: config with monorepo.tag_prefix + dir
        // → context creation → populate_git_vars → verify ALL template vars.
        let mut config = Config::default();
        config.project_name = "mymonorepo".to_string();
        config.monorepo = Some(crate::config::MonorepoConfig {
            tag_prefix: Some("services/api/".to_string()),
            dir: Some("services/api".to_string()),
        });

        // Verify Config helper methods work
        assert_eq!(config.monorepo_tag_prefix(), Some("services/api/"));
        assert_eq!(config.monorepo_dir(), Some("services/api"));

        let mut ctx = Context::new(config, ContextOptions::default());

        // Simulate git info as it would appear in a monorepo:
        // tag and summary already contain the prefix from git.
        let mut info = make_git_info(false, None);
        info.tag = "services/api/v2.1.0".to_string();
        info.previous_tag = Some("services/api/v2.0.5".to_string());
        info.summary = "services/api/v2.1.0-0-gabc123d".to_string();
        info.semver = crate::git::SemVer {
            major: 2,
            minor: 1,
            patch: 0,
            prerelease: None,
            build_metadata: None,
        };
        ctx.git_info = Some(info);
        ctx.populate_git_vars();

        let v = ctx.template_vars();

        // Base vars should have the prefix STRIPPED.
        assert_eq!(v.get("Tag"), Some(&"v2.1.0".to_string()));
        assert_eq!(v.get("Version"), Some(&"2.1.0".to_string()));
        assert_eq!(v.get("RawVersion"), Some(&"2.1.0".to_string()));
        assert_eq!(v.get("Major"), Some(&"2".to_string()));
        assert_eq!(v.get("Minor"), Some(&"1".to_string()));
        assert_eq!(v.get("Patch"), Some(&"0".to_string()));
        assert_eq!(v.get("PreviousTag"), Some(&"v2.0.5".to_string()));
        assert_eq!(v.get("Summary"), Some(&"v2.1.0-0-gabc123d".to_string()));

        // Prefixed vars should retain the FULL prefix.
        assert_eq!(
            v.get("PrefixedTag"),
            Some(&"services/api/v2.1.0".to_string())
        );
        assert_eq!(
            v.get("PrefixedPreviousTag"),
            Some(&"services/api/v2.0.5".to_string())
        );
        assert_eq!(
            v.get("PrefixedSummary"),
            Some(&"services/api/v2.1.0-0-gabc123d".to_string())
        );

        // Project name should be available.
        assert_eq!(v.get("ProjectName"), Some(&"mymonorepo".to_string()));
    }
}
