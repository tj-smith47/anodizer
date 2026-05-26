use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::archives::ContentSource;
use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// ChangelogConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
    /// (GoReleaser uses a plain string here; anodizer extends to ContentSource
    /// for consistency with `release.header`).
    pub header: Option<ContentSource>,
    /// Text appended to the changelog. Same shape as `header`.
    pub footer: Option<ContentSource>,
    /// Skip changelog generation. Accepts bool or template string
    /// (e.g. `"{{ if IsSnapshot }}true{{ endif }}"` for conditional skip).
    ///
    /// Accepts `disable:` as an alias so GoReleaser configs (which use
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
    /// to N chars. Values below `-1` are clamped to `-1` for parity with
    /// GoReleaser (whose `git log --abbrev=N` panics for `-2`, `-3`, ...).
    /// Mirrors GoReleaser `internal/pipe/changelog/changelog.go`'s
    /// `abbrevEntry`.
    pub abbrev: Option<i32>,
    /// Template for each changelog commit line. Available variables: SHA (full hash), ShortSHA (abbreviated), Message (commit subject), AuthorName, AuthorEmail, Login (per-commit GitHub username, `github` backend only), Logins (per-entry comma-separated list of GitHub usernames for that commit, `github` backend only), AllLogins (comma-separated list of all GitHub usernames across the entire release, `github` backend only).<br><br>Default depends on backend (mirrors GoReleaser `internal/pipe/changelog/changelog.go`'s `formatEntry`, which uses the full SHA):<br>&bull; `git` backend (default): `"{{ SHA }} {{ Message }}"`<br>&bull; `github`/`gitlab`/`gitea` backend: `"{{ SHA }}: {{ Message }} (@Login or AuthorName <AuthorEmail>)"` — falls back to `AuthorName <AuthorEmail>` when `Login` is empty.<br><br>When `abbrev < 0`, the default reduces to `"{{ Message }}"` (no hash prefix).
    pub format: Option<String>,
    /// File paths to filter commits by. Only commits touching files under these
    /// paths are included. Works with `use: git` for precise per-commit filtering.
    /// With `use: github`, only the first path is used for API queries; multi-path
    /// filtering is coarse. Supports template rendering.
    pub paths: Option<Vec<String>>,
    /// Title heading for the changelog. Default: "Changelog". Supports templates.
    pub title: Option<String>,
    /// Divider string inserted between changelog groups (e.g. `"---"`). Supports templates.
    pub divider: Option<String>,
    /// AI-powered changelog enhancement configuration.
    pub ai: Option<ChangelogAiConfig>,
    /// When `true`, render the changelog even in snapshot mode. Anodizer
    /// matches GoReleaser's default (skip changelog on `ctx.Snapshot`) and
    /// lets users opt back in here for local preview / draft generation.
    /// Wired in `crates/stage-changelog/src/lib.rs::ChangelogStage::run`.
    pub snapshot: Option<bool>,
}

impl ChangelogConfig {
    /// Default `sort` value. Empty string means "preserve commit order"
    /// (no sort applied). Mirrors GoReleaser `changelog.go`'s
    /// `checkSortDirection`, which accepts "", "asc", "desc".
    pub const DEFAULT_SORT: &'static str = "";

    /// Valid `sort` values. Anything else is a config error.
    pub const VALID_SORT: &[&'static str] = &["", "asc", "desc"];

    /// Default changelog source. Mirrors GoReleaser `changelog.go` —
    /// the unset field falls back to git log parsing.
    pub const DEFAULT_USE_SOURCE: &'static str = "git";

    /// Valid `use:` values. github-native delegates to GitHub's
    /// auto-generated notes; the others control which API anodize hits
    /// for commit metadata.
    pub const VALID_USE_SOURCE: &[&'static str] =
        &["git", "github", "gitlab", "gitea", "github-native"];

    /// Default changelog title heading. Mirrors GoReleaser `changelog.go`
    /// (always emits a `## Changelog` heading when title is unset).
    pub const DEFAULT_TITLE: &'static str = "Changelog";

    /// Default `abbrev` (hash truncation length). 0 = full SHA, mirroring
    /// GoReleaser `abbrevEntry`. The misleading "Default: 7" docstring
    /// previously on the field has been corrected.
    pub const DEFAULT_ABBREV: i32 = 0;

    /// Default `format` template when `abbrev` is negative (hash omitted).
    pub const DEFAULT_FORMAT_NO_HASH: &'static str = "{{ Message }}";

    /// Default `format` template for SCM-backed sources (github/gitlab/gitea).
    /// Renders SHA, message, and an `@login` mention falling back to
    /// `AuthorName <AuthorEmail>` when the API returned no login.
    pub const DEFAULT_FORMAT_SCM: &'static str = "{{ SHA }}: {{ Message }} ({% if Login %}@{{ Login }}{% else %}{{ AuthorName }} <{{ AuthorEmail }}>{% endif %})";

    /// Default `format` template for the `git` backend. Mirrors GoReleaser.
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
    /// Mirrors GoReleaser `internal/pipe/changelog/changelog.go` (commit
    /// 88daaf3): values below `-1` are clamped to `-1`. Upstream's `git log
    /// --abbrev=N` panics for `-2`, `-3`, etc.; anodizer renders SHAs in
    /// Rust so it would not panic, but we still clamp for behavioural parity
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

    /// Resolve `snapshot`, falling back to `false` (matches GoReleaser:
    /// skip changelog on `ctx.Snapshot`).
    pub fn resolved_snapshot(&self) -> bool {
        self.snapshot.unwrap_or(false)
    }
}

/// AI-powered changelog enhancement configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ChangelogAiConfig {
    /// AI provider to use. Valid: "anthropic", "openai", "ollama".
    /// Empty disables the feature.
    #[serde(rename = "use")]
    pub provider: Option<String>,
    /// Model name (e.g. "gpt-4", "claude-sonnet-4-20250514"). Defaults to provider's default.
    pub model: Option<String>,
    /// Prompt template for the AI. Can be a string, or use `from_url`/`from_file`.
    /// Template variable `.ReleaseNotes` contains the current changelog.
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
#[serde(default)]
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
#[serde(default)]
pub struct ContentFromUrl {
    /// URL to fetch (supports templates).
    pub url: Option<String>,
    /// HTTP headers to send with the request.
    pub headers: Option<std::collections::HashMap<String, String>>,
}

/// Load content from a local file.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
#[serde(default)]
pub struct ChangelogFilters {
    /// Regex patterns: commits matching any of these are excluded from the changelog.
    pub exclude: Option<Vec<String>>,
    /// Regex patterns: only commits matching at least one of these are included.
    pub include: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
