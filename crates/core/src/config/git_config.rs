use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GitConfig
// ---------------------------------------------------------------------------

/// Git-level tag discovery and sorting settings.
///
/// Controls how anodizer discovers and orders tags when determining the current
/// and previous versions. This is separate from `TagConfig`, which controls
/// version *bumping* logic.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GitConfig {
    /// How to sort git tags when determining the latest version.
    ///
    /// Accepted values:
    /// - `"-version:refname"` (default) — lexicographic version sort on the tag name.
    /// - `"-version:creatordate"` — sort by the tag's creation date (newest first).
    /// - `"semver"` — strict SemVer 2.0.0 ordering computed in Rust; prereleases
    ///   sort below their release per spec section 11. Bypasses git's native sort.
    /// - `"smartsemver"` — same ordering as `"semver"`, but when the current
    ///   version (resolved from the template `Version` variable) is non-prerelease,
    ///   prerelease tags are filtered out before previous-tag selection. Prevents
    ///   `v0.2.0-beta.3` from being picked as the predecessor of `v0.2.0` (which
    ///   would otherwise produce an empty changelog).
    pub tag_sort: Option<String>,
    /// Tag patterns to ignore during version detection (supports templates).
    /// Tags matching any pattern in this list are excluded from version
    /// detection entirely.
    pub ignore_tags: Option<Vec<String>>,
    /// Tag prefixes to ignore during version detection (supports templates).
    /// Tags starting with any prefix in this list are excluded.
    /// Mirrors GoReleaser Pro's ignore_tag_prefixes feature.
    pub ignore_tag_prefixes: Option<Vec<String>>,
    /// Suffix that identifies pre-release tags for sorting purposes.
    /// When set, tags ending with this suffix are treated as pre-releases
    /// and sorted accordingly during tag discovery.
    pub prerelease_suffix: Option<String>,
}
