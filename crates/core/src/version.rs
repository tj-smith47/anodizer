//! Release-version classification.
//!
//! A "release version" is one safe to ship to an external, often
//! irreversible, package index (crates.io, Cloudsmith, Chocolatey,
//! winget, AUR, …). A snapshot / dev / `0.0.0`-sentinel version is NOT:
//! shipping it is essentially always a mistake, and several index
//! publishers are one-way doors. This module is the single source of
//! truth for that predicate so the publish / blob / announce stages
//! cannot drift on what counts as "non-release".

/// Returns `true` when `version` is safe to publish to an external index —
/// i.e. it is NOT a snapshot / dirty / `0.0.0`-sentinel marker.
///
/// A version is classified **non-release** (returns `false`) when, after
/// trimming, ANY of the following hold:
///
/// - it is empty (no version resolved at all), OR
/// - it matches the `0.0.0` missing-version sentinel — `0.0.0` optionally
///   followed by a `-`, `+`, or `~` pre-release / build / packaging suffix
///   (`0.0.0`, `0.0.0-SNAPSHOT-abc`, `0.0.0~SNAPSHOT_abc`, `0.0.0+dirty`), OR
/// - it carries a snapshot / dirty marker anywhere in the string —
///   `SNAPSHOT` (case-insensitive) or a `dirty` git-state marker.
///
/// Conventional semver pre-releases (`-rc`, `-beta`, `-alpha`, `-dev`) are
/// genuine releases: a `-dev` pre-release is the same release class as `-rc`
/// and is published deliberately, so it is NOT blocked. The real accident this
/// guards (`0.0.0~SNAPSHOT-<sha>`) is already caught by both the `0.0.0`
/// sentinel and the `SNAPSHOT` marker.
///
/// The check is intentionally substring/prefix based rather than strict
/// semver parsing: the synthesized snapshot version
/// (`<base>-SNAPSHOT-<sha>`) and the AUR `~`-normalized form
/// (`0.0.0~SNAPSHOT_<sha>`) are both *valid-enough* strings that a naive
/// `parse_semver` would accept, yet neither must ever reach a real index.
pub fn is_release_version(version: &str) -> bool {
    non_release_reason(version).is_none()
}

/// The human-readable reason `version` is non-release, or `None` when it is a
/// genuine release version. Drives the publish guard's error message so the
/// operator sees *why* the version was rejected, not just that it was.
pub fn non_release_reason(version: &str) -> Option<&'static str> {
    let v = version.trim();
    if v.is_empty() {
        return Some("no version resolved (empty)");
    }
    if is_zero_sentinel(v) {
        return Some("0.0.0 missing-version sentinel");
    }
    let lower = v.to_ascii_lowercase();
    if lower.contains("snapshot") {
        return Some("snapshot marker");
    }
    if lower.contains("dirty") {
        return Some("git-dirty marker");
    }
    None
}

/// `0.0.0` exactly, or `0.0.0` followed by a `-` / `+` / `~` suffix.
fn is_zero_sentinel(v: &str) -> bool {
    let Some(rest) = v.strip_prefix("0.0.0") else {
        return false;
    };
    rest.is_empty() || matches!(rest.as_bytes()[0], b'-' | b'+' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_semver_is_a_release_version() {
        for v in ["1.0.0", "0.2.1", "10.20.30", "1.2.3-rc.1", "2.0.0+build.5"] {
            assert!(is_release_version(v), "{v} should be a release version");
            assert_eq!(non_release_reason(v), None, "{v}");
        }
    }

    #[test]
    fn empty_version_is_non_release() {
        assert!(!is_release_version(""));
        assert!(!is_release_version("   "));
        assert_eq!(non_release_reason(""), Some("no version resolved (empty)"));
    }

    #[test]
    fn zero_sentinel_is_non_release() {
        for v in [
            "0.0.0",
            "0.0.0-SNAPSHOT-d7813f0",
            "0.0.0~SNAPSHOT_d7813f0",
            "0.0.0+dirty",
        ] {
            assert!(!is_release_version(v), "{v} must be non-release");
        }
        assert_eq!(
            non_release_reason("0.0.0"),
            Some("0.0.0 missing-version sentinel")
        );
        // A non-zero version sharing the 0.0.0 *digits* prefix-by-accident must
        // NOT false-trip on the sentinel (it is caught by length/format).
        assert!(is_release_version("0.0.01")); // 0.0.01 does not strip to a suffix sep
    }

    #[test]
    fn snapshot_marker_is_non_release() {
        for v in ["1.2.3-SNAPSHOT-abc", "1.2.3-snapshot-abc", "9.9.9-SNAPSHOT"] {
            assert!(!is_release_version(v), "{v} must be non-release");
            assert_eq!(non_release_reason(v), Some("snapshot marker"), "{v}");
        }
    }

    #[test]
    fn dirty_marker_is_non_release() {
        assert!(!is_release_version("1.2.3+dirty"));
        assert!(!is_release_version("1.2.3-20240101-dirty"));
        assert_eq!(non_release_reason("1.2.3+dirty"), Some("git-dirty marker"));
    }

    #[test]
    fn dev_pre_release_is_a_release_version() {
        // `-dev` is a conventional semver pre-release, the same class as
        // `-rc` / `-beta`, and is published deliberately. Blocking it while
        // allowing `-rc` would be arbitrary; the real accident
        // (`0.0.0~SNAPSHOT`) is caught by the 0.0.0 sentinel + SNAPSHOT marker.
        for v in ["1.0.0-dev.1", "1.2.3-dev", "1.2.3.dev5", "1.0.0-alpha.dev"] {
            assert!(is_release_version(v), "{v} must be a release version");
            assert_eq!(non_release_reason(v), None, "{v}");
        }
    }

    /// Pins the corrected predicate so the deliberate `-dev` decision and the
    /// kept anodizer-specific markers cannot silently regress.
    #[test]
    fn predicate_marker_list_is_pinned() {
        // PASS — genuine releases, including every conventional pre-release.
        for v in [
            "1.0.0-dev.1",
            "1.0.0-rc.1",
            "2.0.0-beta",
            "1.0.0-alpha.dev",
            "1.2.3+build.5",
        ] {
            assert!(is_release_version(v), "{v} must be a release version");
        }
        // FAIL — anodizer's non-release signals: empty, 0.0.0 sentinel,
        // SNAPSHOT marker, dirty.
        for v in [
            "",
            "0.0.0",
            "0.0.0-rc.1",
            "0.9.0-SNAPSHOT-abc123",
            "1.2.3+dirty",
        ] {
            assert!(!is_release_version(v), "{v} must be non-release");
        }
    }
}
