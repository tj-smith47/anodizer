use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

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

/// Prepend `monorepo.dir` to a relative path, leaving absolute paths,
/// templates, and already-prefixed paths alone.
///
/// Mirrors GoReleaser Pro's documented contract: "Extra files on the
/// release, archives, Docker builds, etc are prefixed with `monorepo.dir`."
/// (`www/content/customization/monorepo.md:49-50`)
///
/// Rules:
/// - Empty `dir` or empty `path` → no change.
/// - Absolute paths (`/foo`, `C:\foo`, `\\?\foo`) → no change.
/// - Template strings (start with `{{` after optional whitespace) → no change.
///   Users who template the head of a path are expressing explicit intent.
/// - Paths already starting with `<dir>/` or `<dir>\` → no change.
/// - Paths equal to `<dir>` → no change.
/// - Otherwise: prepend `<dir>/`.
///
/// Returns `None` when the input would be unchanged (so callers can keep
/// the original string and avoid spurious cloning).
pub fn prepend_monorepo_dir(path: &str, dir: &str) -> Option<String> {
    if dir.is_empty() || path.is_empty() {
        return None;
    }
    // Templated paths are user-shaped; do not mutate.
    if path.trim_start().starts_with("{{") {
        return None;
    }
    if Path::new(path).is_absolute() {
        return None;
    }
    // Windows-style drive paths (`C:\…`) are not flagged absolute on
    // unix; check the colon-on-second-char form too so cross-platform
    // configs survive the prefix pass.
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }
    // Already prefixed?
    let trimmed_dir = dir.trim_end_matches(['/', '\\']);
    if path == trimmed_dir {
        return None;
    }
    if let Some(rest) = path.strip_prefix(trimmed_dir)
        && (rest.starts_with('/') || rest.starts_with('\\'))
    {
        return None;
    }
    Some(format!("{}/{}", trimmed_dir, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepends_relative_path() {
        assert_eq!(
            prepend_monorepo_dir("LICENSE", "sub1").as_deref(),
            Some("sub1/LICENSE")
        );
    }

    #[test]
    fn leaves_absolute_path() {
        assert_eq!(prepend_monorepo_dir("/etc/passwd", "sub1"), None);
    }

    #[test]
    fn leaves_windows_drive_path() {
        assert_eq!(prepend_monorepo_dir("C:\\Users\\me", "sub1"), None);
    }

    #[test]
    fn leaves_already_prefixed() {
        assert_eq!(prepend_monorepo_dir("sub1/LICENSE", "sub1"), None);
        assert_eq!(prepend_monorepo_dir("sub1/", "sub1"), None);
        assert_eq!(prepend_monorepo_dir("sub1", "sub1"), None);
    }

    #[test]
    fn dir_with_trailing_slash_normalised() {
        assert_eq!(
            prepend_monorepo_dir("LICENSE", "sub1/").as_deref(),
            Some("sub1/LICENSE")
        );
    }

    #[test]
    fn leaves_template_paths() {
        assert_eq!(prepend_monorepo_dir("{{ .ProjectDir }}/x", "sub1"), None);
        assert_eq!(prepend_monorepo_dir(" {{ .X }}", "sub1"), None);
    }

    #[test]
    fn handles_glob_patterns() {
        assert_eq!(
            prepend_monorepo_dir("*.txt", "sub1").as_deref(),
            Some("sub1/*.txt")
        );
        assert_eq!(
            prepend_monorepo_dir("dist/**/*.so", "sub1").as_deref(),
            Some("sub1/dist/**/*.so")
        );
    }

    #[test]
    fn no_change_for_empty_inputs() {
        assert_eq!(prepend_monorepo_dir("", "sub1"), None);
        assert_eq!(prepend_monorepo_dir("LICENSE", ""), None);
    }
}
