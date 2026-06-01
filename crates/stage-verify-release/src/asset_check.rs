//! Pure asset-existence diff.
//!
//! The post-release asset-existence check compares the set of artifacts
//! anodizer PRODUCED (and intended to upload) against the set of assets the
//! published release actually STORES. GitHub silently tolerates partial
//! uploads — a 422/transient that drops one asset can leave a release missing
//! a checksum or a `.deb` while every other asset is present. This diff
//! surfaces exactly which produced artifacts have no matching uploaded asset.
//!
//! The function is pure (no I/O) so it is unit-testable without the network:
//! the live asset list is fetched by
//! [`anodizer_stage_release::fetch_published_asset_names`] at the call site
//! and passed in here as a plain list.

/// Outcome of diffing produced artifacts against published release assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetDiff {
    /// Produced artifact names with NO matching uploaded asset — the defects
    /// the check exists to catch (partial uploads).
    pub missing: Vec<String>,
    /// Uploaded assets that do NOT correspond to any produced artifact —
    /// orphans (e.g. a stale asset from a prior re-cut). Reported as an
    /// advisory, never a failure on its own.
    pub orphan: Vec<String>,
}

impl AssetDiff {
    /// Whether any produced artifact is missing from the published release.
    /// Orphans alone do NOT make the diff a failure.
    pub fn has_missing(&self) -> bool {
        !self.missing.is_empty()
    }
}

/// Diff `produced` artifact names against `published` (uploaded) asset names.
///
/// Both lists are compared by exact name. The result lists, sorted and
/// de-duplicated, the produced names absent from `published` (`missing`) and
/// the published names absent from `produced` (`orphan`).
pub fn diff_assets(produced: &[String], published: &[String]) -> AssetDiff {
    use std::collections::BTreeSet;
    let produced_set: BTreeSet<&str> = produced.iter().map(String::as_str).collect();
    let published_set: BTreeSet<&str> = published.iter().map(String::as_str).collect();

    let missing = produced_set
        .difference(&published_set)
        .map(|s| s.to_string())
        .collect();
    let orphan = published_set
        .difference(&produced_set)
        .map(|s| s.to_string())
        .collect();

    AssetDiff { missing, orphan }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reports_missing_produced_artifact() {
        // produced {a,b,c} vs published {a,c} => b missing.
        let diff = diff_assets(&v(&["a", "b", "c"]), &v(&["a", "c"]));
        assert_eq!(diff.missing, v(&["b"]));
        assert!(diff.orphan.is_empty());
        assert!(diff.has_missing());
    }

    #[test]
    fn all_present_passes() {
        let diff = diff_assets(&v(&["a", "b", "c"]), &v(&["a", "b", "c"]));
        assert!(diff.missing.is_empty());
        assert!(diff.orphan.is_empty());
        assert!(!diff.has_missing());
    }

    #[test]
    fn extra_published_asset_is_orphan_not_failure() {
        let diff = diff_assets(&v(&["a", "b"]), &v(&["a", "b", "stale.txt"]));
        assert!(diff.missing.is_empty());
        assert_eq!(diff.orphan, v(&["stale.txt"]));
        assert!(
            !diff.has_missing(),
            "orphans alone must not fail the asset check"
        );
    }

    #[test]
    fn missing_and_orphan_both_reported() {
        let diff = diff_assets(&v(&["a", "b"]), &v(&["a", "x"]));
        assert_eq!(diff.missing, v(&["b"]));
        assert_eq!(diff.orphan, v(&["x"]));
        assert!(diff.has_missing());
    }

    #[test]
    fn results_are_sorted_and_deduped() {
        let diff = diff_assets(&v(&["c", "a", "a", "b"]), &v(&["a"]));
        assert_eq!(diff.missing, v(&["b", "c"]), "sorted + deduped");
    }

    #[test]
    fn empty_produced_set_has_no_missing() {
        let diff = diff_assets(&[], &v(&["a", "b"]));
        assert!(diff.missing.is_empty());
        assert_eq!(diff.orphan, v(&["a", "b"]));
    }
}
