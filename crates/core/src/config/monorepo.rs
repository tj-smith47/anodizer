use super::*;

// ---------------------------------------------------------------------------
// MonorepoConfig
// ---------------------------------------------------------------------------

/// GoReleaser Pro monorepo configuration.
///
/// When configured, tag discovery filters by `tag_prefix` and the working
/// directory is scoped to `dir`.
///
/// This is DIFFERENT from `TagConfig.tag_prefix`:
/// - `MonorepoConfig.tag_prefix`: tags in git already HAVE the prefix
///   (e.g. `subproject1/v1.2.3`). The prefix is STRIPPED for `{{ .Tag }}`
///   while `{{ .PrefixedTag }}` retains the full tag.
/// - `TagConfig.tag_prefix`: a prefix to PREPEND when constructing
///   `{{ .PrefixedTag }}` from a plain tag.
///
/// When `monorepo` is configured, it takes precedence over `tag.tag_prefix`
/// for `PrefixedTag` / `PrefixedPreviousTag` behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MonorepoConfig {
    /// Tag prefix for this subproject (e.g. `"subproject1/"`).
    ///
    /// Tags matching this prefix are selected during tag discovery, and the
    /// prefix is stripped from `{{ .Tag }}` while `{{ .PrefixedTag }}` retains
    /// the full tag.
    pub tag_prefix: Option<String>,
    /// Working directory for this subproject.
    ///
    /// Used for changelog path filtering (when no explicit `changelog.paths`
    /// or `crate.path` is configured) and as the default build `dir`.
    pub dir: Option<String>,
}
