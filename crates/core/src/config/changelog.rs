use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::archives::ContentSource;
use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogConfig {
    /// Sort order for changelog entries: "asc" or "desc" (default: "asc").
    pub sort: Option<String>,
    /// Commit message filters to include or exclude from the changelog.
    pub filters: Option<ChangelogFilters>,
    /// Groups for organizing changelog entries by commit message prefix.
    pub groups: Option<Vec<ChangelogGroup>>,
    /// Text prepended to the changelog. Inline string, `from_file: <path>`,
    /// or `from_url: <url>` — symmetric with the release block's header/footer
    /// so users can compose headers from a templated file or remote endpoint
    /// (the upstream uses a plain string here; anodizer extends to ContentSource
    /// for consistency with `release.header`).
    pub header: Option<ContentSource>,
    /// Text appended to the changelog. Same shape as `header`.
    pub footer: Option<ContentSource>,
    /// Skip changelog generation. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    ///
    /// Accepts `disable:` as an alias so imported configs (which use
    /// `changelog.disable:`) parse cleanly without a rename. Anodizer's
    /// broader convention is `skip:` (mirrors `release.skip_upload`,
    /// stage-level `skip:` flags), so the canonical key stays `skip:`.
    #[serde(
        deserialize_with = "deserialize_string_or_bool_opt",
        default,
        alias = "disable"
    )]
    pub skip: Option<StringOrBool>,
    /// Changelog source: `"git"` (default), `"github"`, or `"github-native"`.
    /// `"github"` fetches commits via the GitHub API, enriching entries with
    /// author login information (available as the `{{ Logins }}` per-entry
    /// template variable and the `{{ AllLogins }}` release-wide variable).
    /// `"github-native"` delegates entirely to GitHub's auto-generated notes.
    #[serde(rename = "use")]
    pub use_source: Option<String>,
    /// Hash abbreviation length. Default: 0 (no truncation, emit the full
    /// SHA). Set to -1 to omit the hash entirely; positive values truncate
    /// to N chars. Values below `-1` are clamped to `-1` (a `git log
    /// --abbrev=N` would otherwise reject `-2`, `-3`, ...).
    pub abbrev: Option<i32>,
    /// Template for each changelog commit line. Available variables: SHA (full hash), ShortSHA (abbreviated), Message (commit subject), AuthorName, AuthorEmail, Login (per-commit GitHub username), Logins (per-entry comma-separated list of GitHub usernames for that commit), AllLogins (comma-separated list of all GitHub usernames across the entire release), AuthorUsername (renders `@login` when the login is known, the plain author name otherwise).<br><br>Logins come from the SCM API backends (`use: github`/`gitea`) and — when the release targets GitHub and a token is available — from GitHub-API enrichment of the default `git` backend, so `use: git` changelogs render `@login` mentions too. Release bodies carry bare `@login` (GitHub autolinks them); on-disk `CHANGELOG.md` files get explicit `[@login](https://github.com/login)` links. Without a token (or offline, or with a non-GitHub remote) rendering keeps the plain author name.<br><br>Default depends on backend (the full SHA is used):<br>&bull; `git` backend (default): `"{{ SHA }} {{ Message }}"`<br>&bull; `github`/`gitlab`/`gitea` backend: `"{{ SHA }}: {{ Message }} (@Login or AuthorName <AuthorEmail>)"` — falls back to `AuthorName <AuthorEmail>` when `Login` is empty.<br><br>When `abbrev < 0`, the default reduces to `"{{ Message }}"` (no hash prefix).
    pub format: Option<String>,
    /// Optional path filter that NARROWS the per-crate scope by intersection —
    /// it never replaces it. Each changelog track already scopes to its own
    /// commits (a per-crate track to its crate directory; the aggregate to the
    /// union of every crate directory plus the workspace manifests). When set,
    /// `paths` further restricts that derived scope to commits whose touched
    /// files match these globs; it can only ever drop commits, never widen to
    /// another track's directory. A `paths` value that is a superset of the
    /// derived scope (e.g. `["crates/**", "Cargo.toml", "Cargo.lock"]` over a
    /// workspace) is therefore a no-op — and so is the recommended default of
    /// leaving `paths` unset, where scoping is fully derived. The same derived
    /// scope and intersect drive all three changelog formats
    /// (`keep-a-changelog`, `json`, and `release-notes`), so they cannot drift.
    ///
    /// With `use: git` the intersect is precise (commits are filtered by their
    /// touched files). With `use: github` only the first path is used for API
    /// queries; with `use: gitlab` / `gitea` path filtering is unsupported, so a
    /// narrowing `paths` there is coarse and a warning is emitted. Supports
    /// template rendering.
    pub paths: Option<Vec<String>>,
    /// Title heading for the changelog. Default: "Changelog". Supports templates.
    pub title: Option<String>,
    /// Divider string inserted between changelog groups (e.g. `"---"`). Supports templates.
    pub divider: Option<String>,
    /// AI-powered changelog enhancement configuration.
    pub ai: Option<ChangelogAiConfig>,
    /// When `true`, render the changelog even in snapshot mode. Anodizer
    /// matches the default (skip changelog on snapshot) and
    /// lets users opt back in here for local preview / draft generation.
    /// Wired in `crates/stage-changelog/src/lib.rs::ChangelogStage::run`.
    pub snapshot: Option<bool>,
    /// Changelog file-layout controls: which `CHANGELOG.md` files a release
    /// writes (per-crate vs the aggregate root). Separate from the
    /// content-generation keys above (`use`, `format`, `groups`, `filters`,
    /// `paths`, `sort`, ...) so file management and content concerns stay
    /// orthogonal. See [`ChangelogFilesConfig`].
    pub files: Option<ChangelogFilesConfig>,
}

/// Changelog file-layout controls: which `CHANGELOG.md` files a release writes.
///
/// Nested under `changelog.files` to keep file-management keys distinct from
/// the content-generation keys on [`ChangelogConfig`] (`use`, `format`,
/// `groups`, `filters`, `paths`, `sort`, ...).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogFilesConfig {
    /// When `true`, write a per-crate `crates/<name>/CHANGELOG.md` for each
    /// crate instead of (or in addition to) the single root `CHANGELOG.md`.
    /// Default: `false`. With `per_crate: true` and no `root:` block, only the
    /// per-crate files are produced; add a `root:` block to keep the aggregate
    /// root changelog as well.
    pub per_crate: Option<bool>,
    /// Controls the single aggregate root `CHANGELOG.md`. Presence of this
    /// block forces the root changelog on even when `per_crate: true`. When
    /// omitted, the root changelog is on unless `per_crate: true` turned it off.
    pub root: Option<RootChangelogConfig>,
}

impl ChangelogConfig {
    /// Default `sort` value. Empty string means "preserve commit order"
    /// (no sort applied). The
    /// `checkSortDirection`, which accepts "", "asc", "desc".
    pub const DEFAULT_SORT: &'static str = "";

    /// Valid `sort` values. Anything else is a config error.
    pub const VALID_SORT: &[&'static str] = &["", "asc", "desc"];

    /// Default changelog source —
    /// the unset field falls back to git log parsing.
    pub const DEFAULT_USE_SOURCE: &'static str = "git";

    /// Valid `use:` values. github-native delegates to GitHub's
    /// auto-generated notes; the others control which API anodize hits
    /// for commit metadata.
    pub const VALID_USE_SOURCE: &[&'static str] =
        &["git", "github", "gitlab", "gitea", "github-native"];

    /// Default changelog title heading
    /// (always emits a `## Changelog` heading when title is unset).
    pub const DEFAULT_TITLE: &'static str = "Changelog";

    /// Default `abbrev` (hash truncation length). 0 = full SHA, mirroring
    /// the abbrev-entry behaviour. The misleading "Default: 7" docstring
    /// previously on the field has been corrected.
    pub const DEFAULT_ABBREV: i32 = 0;

    /// Default `format` template when `abbrev` is negative (hash omitted).
    pub const DEFAULT_FORMAT_NO_HASH: &'static str = "{{ Message }}";

    /// Default `format` template for SCM-backed sources (github/gitlab/gitea).
    /// Renders SHA, message, and an `@login` mention falling back to
    /// `AuthorName <AuthorEmail>` when the API returned no login.
    pub const DEFAULT_FORMAT_SCM: &'static str = "{{ SHA }}: {{ Message }} ({% if Login %}@{{ Login }}{% else %}{{ AuthorName }} <{{ AuthorEmail }}>{% endif %})";

    /// Default `format` template for the `git` backend.
    pub const DEFAULT_FORMAT_GIT: &'static str = "{{ SHA }} {{ Message }}";

    /// Resolve the `sort` mode, falling back to [`Self::DEFAULT_SORT`]
    /// (empty = preserve commit order). Returns an error when the user
    /// supplied a value outside [`Self::VALID_SORT`] so the invalid mode
    /// surfaces at the call site.
    pub fn resolved_sort(&self) -> anyhow::Result<&str> {
        let value = self.sort.as_deref().unwrap_or(Self::DEFAULT_SORT);
        if Self::VALID_SORT.contains(&value) {
            Ok(value)
        } else {
            Err(anyhow::anyhow!(
                "changelog: invalid sort '{}', must be one of: \"\", asc, desc",
                value
            ))
        }
    }

    /// Resolve the changelog source, falling back to `"git"`.
    pub fn resolved_use_source(&self) -> &str {
        self.use_source
            .as_deref()
            .unwrap_or(Self::DEFAULT_USE_SOURCE)
    }

    /// Resolve the title heading, falling back to `"Changelog"`. An empty
    /// `title:` is preserved (the renderer skips emitting the heading
    /// when the resolved title is empty), so the schema still allows
    /// users to suppress the heading with an explicit empty string.
    pub fn resolved_title(&self) -> &str {
        self.title.as_deref().unwrap_or(Self::DEFAULT_TITLE)
    }

    /// Resolve `abbrev`, falling back to [`Self::DEFAULT_ABBREV`] (0 = full SHA).
    ///
    /// Values below `-1` are clamped to `-1`. A `git log
    /// --abbrev=N` would reject `-2`, `-3`, etc.; anodizer renders SHAs in
    /// Rust so it would not fail, but still clamps for behavioural parity
    /// — a configuration like `abbrev: -5` produces the same "omit hash"
    /// output as `abbrev: -1`.
    pub fn resolved_abbrev(&self) -> i32 {
        self.abbrev.unwrap_or(Self::DEFAULT_ABBREV).max(-1)
    }

    /// Resolve the per-entry `format:` template. When the user did not
    /// set `format:`, returns the backend-specific default keyed off
    /// `use_source` and `abbrev` (negative abbrev → no-hash template;
    /// SCM backend → SCM template; git backend → SHA + message).
    /// Caller should pass the resolved use_source / abbrev values.
    pub fn resolved_format<'a>(&'a self, use_source: &str, abbrev: i32) -> &'a str {
        if let Some(f) = self.format.as_deref() {
            return f;
        }
        if abbrev < 0 {
            return Self::DEFAULT_FORMAT_NO_HASH;
        }
        match use_source {
            "github" | "gitlab" | "gitea" => Self::DEFAULT_FORMAT_SCM,
            _ => Self::DEFAULT_FORMAT_GIT,
        }
    }

    /// Resolve `snapshot`, falling back to `false` (the default:
    /// skip changelog on `ctx.Snapshot`).
    pub fn resolved_snapshot(&self) -> bool {
        self.snapshot.unwrap_or(false)
    }

    /// Whether per-crate `crates/<name>/CHANGELOG.md` files are requested
    /// (`files.per_crate: true`). Defaults to `false`. The single place that
    /// knows `per_crate` lives under `files`.
    pub fn per_crate(&self) -> bool {
        self.files
            .as_ref()
            .and_then(|f| f.per_crate)
            .unwrap_or(false)
    }

    /// The aggregate root `CHANGELOG.md` config block (`files.root`), if set.
    /// The single place that knows `root` lives under `files`.
    pub fn root(&self) -> Option<&RootChangelogConfig> {
        self.files.as_ref().and_then(|f| f.root.as_ref())
    }

    /// Resolve which changelog files this config produces.
    ///
    /// The root changelog is enabled when a `root:` block is present, or when
    /// `per_crate` is not `true` (its default `false` keeps the back-compat
    /// root-only behaviour). Per-crate files are produced exactly when
    /// `per_crate: true`. So a bare `changelog:` block stays root-only, while
    /// `per_crate: true` without `root:` yields per-crate files only.
    pub fn resolved_destination(&self) -> ChangelogDestination {
        let per_crate = self.per_crate();
        ChangelogDestination {
            root_enabled: self.root().is_some() || !per_crate,
            per_crate,
        }
    }

    /// Resolve the ordering of release sections in the root `CHANGELOG.md`,
    /// falling back to [`Chronology::Date`] when `root.chronology` is unset.
    pub fn resolved_chronology(&self) -> Chronology {
        self.root()
            .map(RootChangelogConfig::resolved_chronology)
            .unwrap_or_default()
    }

    /// The optional crate filter for the root changelog: `None` means every
    /// crate contributes a `### <crate>` subsection.
    pub fn root_crates_filter(&self) -> Option<&[String]> {
        self.root().and_then(|r| r.crates.as_deref())
    }
}

/// Ordering of release sections in the aggregate root `CHANGELOG.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Chronology {
    /// Order sections by release date (default).
    #[default]
    Date,
    /// Order sections by semantic tag version.
    Tag,
}

/// Configuration for the single aggregate root `CHANGELOG.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RootChangelogConfig {
    /// Ordering of release sections in the root changelog: `date` (default) or
    /// `tag`.
    pub chronology: Option<Chronology>,
    /// Crates that contribute a `### <crate>` subsection to the root changelog.
    /// When omitted, every crate contributes a subsection.
    pub crates: Option<Vec<String>>,
}

impl RootChangelogConfig {
    /// Resolve the section ordering, falling back to [`Chronology::Date`].
    pub fn resolved_chronology(&self) -> Chronology {
        self.chronology.unwrap_or_default()
    }
}

/// The resolved changelog destination decision: which files a release writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangelogDestination {
    /// Whether the single aggregate root `CHANGELOG.md` is written.
    pub root_enabled: bool,
    /// Whether per-crate `crates/<name>/CHANGELOG.md` files are written.
    pub per_crate: bool,
}

/// AI-powered changelog enhancement configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogAiConfig {
    /// AI provider to use. Valid: "anthropic", "openai", "ollama".
    /// Empty disables the feature.
    #[serde(rename = "use")]
    pub provider: Option<String>,
    /// Model name (e.g. "claude-sonnet-4-6", "gpt-4o-mini", "llama3.1").
    /// Defaults to the provider's default model when unset.
    pub model: Option<String>,
    /// Prompt template for the AI. Can be a string, or use `from_url`/`from_file`.
    /// Template variable `{{ ReleaseNotes }}` contains the current changelog.
    pub prompt: Option<ChangelogAiPrompt>,
}

/// Prompt source for AI changelog: inline string, URL, or file path.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ChangelogAiPrompt {
    /// Inline prompt string (supports templates).
    Inline(String),
    /// Structured prompt with from_url/from_file sources.
    Source(ChangelogAiPromptSource),
}

/// Structured prompt source: load from URL or file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogAiPromptSource {
    /// Load prompt from a URL.
    pub from_url: Option<ContentFromUrl>,
    /// Load prompt from a local file. Overrides from_url if both set.
    pub from_file: Option<ContentFromFile>,
}

/// Resolved prompt source kind after applying priority rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPromptSource {
    /// Load from a local file path.
    File(String),
    /// Load from a URL (with optional headers).
    Url {
        url: String,
        headers: Option<std::collections::HashMap<String, String>>,
    },
    /// No source configured.
    None,
}

impl ChangelogAiPromptSource {
    /// Resolve the prompt source applying priority: from_file overrides from_url.
    pub fn resolve(&self) -> ResolvedPromptSource {
        if let Some(ref file) = self.from_file
            && let Some(ref path) = file.path
        {
            return ResolvedPromptSource::File(path.clone());
        }
        if let Some(ref url_cfg) = self.from_url
            && let Some(ref url) = url_cfg.url
        {
            return ResolvedPromptSource::Url {
                url: url.clone(),
                headers: url_cfg.headers.clone(),
            };
        }
        ResolvedPromptSource::None
    }
}

/// Load content from a URL with optional headers.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContentFromUrl {
    /// URL to fetch (supports templates).
    pub url: Option<String>,
    /// HTTP headers to send with the request.
    pub headers: Option<std::collections::HashMap<String, String>>,
}

/// Load content from a local file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContentFromFile {
    /// Path to the file (supports templates).
    pub path: Option<String>,
}

/// Regex-based commit filters for the changelog stage.
///
/// Patterns are NOT compile-validated at config-load — a malformed regex
/// only surfaces when the changelog stage runs, which on a release pipeline
/// is well past the point of cheap failure. Test patterns locally
/// (`anodizer changelog --check` or any external regex tool) before
/// committing config changes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogFilters {
    /// Regex patterns: commits matching any of these are excluded from the changelog.
    pub exclude: Option<Vec<String>>,
    /// Regex patterns: only commits matching at least one of these are included.
    pub include: Option<Vec<String>>,
    /// Exclude anodizer's own version-sync bump commits
    /// (`chore(release): bump …`, optionally ` [skip ci]`) from the generated
    /// changelog. They are release machinery, not user-facing changes, so this
    /// defaults to `true`. Set `false` to keep them. No effect in include mode
    /// (`include` already drops anything that does not match a pattern).
    pub exclude_version_sync_commits: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChangelogGroup {
    /// Section heading for this group (e.g., "Features", "Bug Fixes").
    pub title: String,
    /// Regex pattern matching commit messages to include in this group.
    pub regexp: Option<String>,
    /// Sort order for this group relative to other groups (lower = first).
    pub order: Option<i32>,
    /// Nested subgroups within this group. Rendered as sub-sections (e.g. `###`).
    pub groups: Option<Vec<ChangelogGroup>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_top_level_field() {
        // The pre-`changelog.files` flat keys (`per_crate:` / `root:` at the
        // changelog top level) must now hard-error rather than be silently
        // ignored — `deny_unknown_fields` is what turns the moved-key migration
        // into a loud, actionable failure instead of changed-output-no-error.
        let yaml = "use: github-native\nper_crate: true\n";
        assert!(serde_yaml_ng::from_str::<ChangelogConfig>(yaml).is_err());
    }

    #[test]
    fn accepts_per_crate_under_files_block() {
        // The supported form (nested under `files:`) still parses.
        let yaml = "use: github-native\nfiles:\n  per_crate: true\n";
        let cfg: ChangelogConfig = serde_yaml_ng::from_str(yaml).expect("nested form parses");
        assert_eq!(cfg.files.and_then(|f| f.per_crate), Some(true));
    }

    #[test]
    fn rejects_unknown_field_under_files_block() {
        let yaml = "files:\n  per_crat: true\n";
        assert!(serde_yaml_ng::from_str::<ChangelogConfig>(yaml).is_err());
    }

    #[test]
    fn destination_bare_config_is_root_only() {
        let cfg = ChangelogConfig::default();
        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled, "bare config keeps the root CHANGELOG.md");
        assert!(!dest.per_crate, "bare config emits no per-crate files");
    }

    #[test]
    fn destination_per_crate_true_without_root_is_per_crate_only() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                per_crate: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let dest = cfg.resolved_destination();
        assert!(
            !dest.root_enabled,
            "per_crate: true without root: drops root"
        );
        assert!(dest.per_crate);
    }

    #[test]
    fn destination_per_crate_true_with_root_is_both() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                per_crate: Some(true),
                root: Some(RootChangelogConfig::default()),
            }),
            ..Default::default()
        };
        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled);
        assert!(dest.per_crate);
    }

    #[test]
    fn destination_per_crate_false_with_root_is_root_only() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                per_crate: Some(false),
                root: Some(RootChangelogConfig::default()),
            }),
            ..Default::default()
        };
        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled);
        assert!(!dest.per_crate);
    }

    #[test]
    fn chronology_defaults_to_date_when_root_unset() {
        let cfg = ChangelogConfig::default();
        assert_eq!(cfg.resolved_chronology(), Chronology::Date);
    }

    #[test]
    fn chronology_defaults_to_date_when_root_set_without_chronology() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                root: Some(RootChangelogConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_chronology(), Chronology::Date);
    }

    #[test]
    fn chronology_override_tag() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                root: Some(RootChangelogConfig {
                    chronology: Some(Chronology::Tag),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_chronology(), Chronology::Tag);
    }

    #[test]
    fn resolved_chronology_accessor_on_root_defaults_to_date() {
        assert_eq!(
            RootChangelogConfig::default().resolved_chronology(),
            Chronology::Date
        );
    }

    #[test]
    fn crates_filter_defaults_to_none_meaning_all() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                root: Some(RootChangelogConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cfg.root_crates_filter(), None);
    }

    #[test]
    fn crates_filter_passes_through_list() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                root: Some(RootChangelogConfig {
                    crates: Some(vec!["a".to_string(), "b".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            cfg.root_crates_filter(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );
    }

    #[test]
    fn deserializes_per_crate_and_root_block() {
        let yaml = r#"
files:
  per_crate: true
  root:
    chronology: tag
    crates: [a, b]
"#;
        let cfg: ChangelogConfig = serde_yaml_ng::from_str(yaml).expect("parse changelog block");
        assert!(cfg.per_crate());
        let root = cfg.root().expect("root present");
        assert_eq!(root.chronology, Some(Chronology::Tag));
        assert_eq!(
            root.crates.as_deref(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );

        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled);
        assert!(dest.per_crate);
        assert_eq!(cfg.resolved_chronology(), Chronology::Tag);
    }

    #[test]
    fn deserializes_bare_block_to_root_only() {
        let yaml = "sort: asc";
        let cfg: ChangelogConfig = serde_yaml_ng::from_str(yaml).expect("parse bare block");
        assert!(!cfg.per_crate());
        assert!(cfg.root().is_none());
        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled);
        assert!(!dest.per_crate);
    }

    #[test]
    fn deserializes_empty_root_block_forces_root_with_default_chronology() {
        let yaml = "files:\n  per_crate: true\n  root: {}\n";
        let cfg: ChangelogConfig = serde_yaml_ng::from_str(yaml).expect("parse");
        let dest = cfg.resolved_destination();
        assert!(dest.root_enabled);
        assert!(dest.per_crate);
        assert_eq!(cfg.resolved_chronology(), Chronology::Date);
    }

    #[test]
    fn empty_crates_list_is_distinct_from_omitted() {
        let with_empty: ChangelogConfig =
            serde_yaml_ng::from_str("files:\n  root:\n    crates: []\n").expect("parse");
        assert_eq!(with_empty.root_crates_filter(), Some(&[][..]));

        let omitted: ChangelogConfig =
            serde_yaml_ng::from_str("files:\n  root: {}\n").expect("parse");
        assert_eq!(omitted.root_crates_filter(), None);
    }

    #[test]
    fn chronology_serde_rename_is_lowercase() {
        assert_eq!(
            serde_yaml_ng::to_string(&Chronology::Date)
                .expect("ser")
                .trim(),
            "date"
        );
        assert_eq!(
            serde_yaml_ng::to_string(&Chronology::Tag)
                .expect("ser")
                .trim(),
            "tag"
        );
    }
}
