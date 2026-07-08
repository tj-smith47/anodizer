//! Pure asset-existence diff and per-asset content comparison.
//!
//! The post-release asset-existence check compares the set of artifacts
//! anodizer PRODUCED (and intended to upload) against the set of assets the
//! published release actually STORES. GitHub silently tolerates partial
//! uploads — a 422/transient that drops one asset can leave a release missing
//! a checksum or a `.deb` while every other asset is present. This diff
//! surfaces exactly which produced artifacts have no matching uploaded asset.
//!
//! [`check_asset_content`] extends the name diff with a byte-level verdict
//! for each asset that IS present: the stored size (and, when GitHub exposes
//! its server-computed `sha256:` digest, the stored digest) must match the
//! local artifact's bytes, catching corrupted uploads and stale assets left
//! over from a prior re-cut.
//!
//! Both functions are pure (no I/O) so they are unit-testable without the
//! network: the live asset list is fetched by
//! [`anodizer_stage_release::fetch_published_assets`] at the call site and
//! passed in here as plain data.

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

/// Digest scheme prefix GitHub uses for its server-computed asset digests.
/// Any other scheme is treated as "no comparable digest" rather than a
/// mismatch, so a future scheme change degrades to the download fallback
/// instead of failing every release.
const SHA256_DIGEST_PREFIX: &str = "sha256:";

/// Byte-level verdict for one uploaded asset vs. its local artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentVerdict {
    /// Size matches AND the stored `sha256:` digest matches the local hash.
    Match,
    /// Stored byte size differs from the local file — the upload is
    /// truncated, corrupted, or a stale asset from a different build.
    SizeMismatch { local: u64, published: u64 },
    /// Sizes agree but the stored digest differs from the local hash —
    /// same-length different bytes (e.g. a stale asset from a prior re-cut).
    DigestMismatch { local: String, published: String },
    /// Size matches but the release carries no comparable `sha256:` digest
    /// (older GHES, or a non-sha256 scheme). The caller decides whether to
    /// fall back to downloading the asset and hashing it.
    DigestUnavailable,
}

/// Compare one published asset's stored size/digest against the local
/// artifact's size and sha256.
///
/// Size is authoritative and checked first: a size mismatch makes the digest
/// comparison meaningless. The published digest is compared only when it uses
/// the `sha256:` scheme; hex comparison is case-insensitive because GitHub's
/// casing is not contractual.
pub fn check_asset_content(
    local_size: u64,
    local_sha256: &str,
    published_size: u64,
    published_digest: Option<&str>,
) -> ContentVerdict {
    if local_size != published_size {
        return ContentVerdict::SizeMismatch {
            local: local_size,
            published: published_size,
        };
    }
    match published_digest.and_then(|d| d.strip_prefix(SHA256_DIGEST_PREFIX)) {
        Some(published_hex) => {
            if published_hex.eq_ignore_ascii_case(local_sha256) {
                ContentVerdict::Match
            } else {
                ContentVerdict::DigestMismatch {
                    local: local_sha256.to_string(),
                    published: published_hex.to_string(),
                }
            }
        }
        None => ContentVerdict::DigestUnavailable,
    }
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

    const SHA_A: &str = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    const SHA_B: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn content_matching_size_and_digest_is_match() {
        let verdict = check_asset_content(100, SHA_A, 100, Some(&format!("sha256:{SHA_A}")));
        assert_eq!(verdict, ContentVerdict::Match);
    }

    #[test]
    fn content_digest_comparison_is_hex_case_insensitive() {
        let upper = format!("sha256:{}", SHA_A.to_ascii_uppercase());
        let verdict = check_asset_content(100, SHA_A, 100, Some(&upper));
        assert_eq!(verdict, ContentVerdict::Match);
    }

    #[test]
    fn content_size_mismatch_reported_with_both_sizes() {
        let verdict = check_asset_content(100, SHA_A, 99, Some(&format!("sha256:{SHA_A}")));
        assert_eq!(
            verdict,
            ContentVerdict::SizeMismatch {
                local: 100,
                published: 99
            }
        );
    }

    #[test]
    fn content_size_mismatch_wins_over_digest_mismatch() {
        // Both wrong: size is authoritative — the digest of a differently-
        // sized blob adds no information.
        let verdict = check_asset_content(100, SHA_A, 99, Some(&format!("sha256:{SHA_B}")));
        assert!(matches!(verdict, ContentVerdict::SizeMismatch { .. }));
    }

    #[test]
    fn content_digest_mismatch_reported_with_both_hashes() {
        let verdict = check_asset_content(100, SHA_A, 100, Some(&format!("sha256:{SHA_B}")));
        assert_eq!(
            verdict,
            ContentVerdict::DigestMismatch {
                local: SHA_A.to_string(),
                published: SHA_B.to_string(),
            }
        );
    }

    #[test]
    fn content_absent_digest_is_unavailable_not_mismatch() {
        let verdict = check_asset_content(100, SHA_A, 100, None);
        assert_eq!(verdict, ContentVerdict::DigestUnavailable);
    }

    #[test]
    fn content_unknown_digest_scheme_is_unavailable_not_mismatch() {
        let verdict = check_asset_content(100, SHA_A, 100, Some("sha512:deadbeef"));
        assert_eq!(verdict, ContentVerdict::DigestUnavailable);
    }
}
