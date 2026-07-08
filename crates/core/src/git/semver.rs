use anyhow::Result;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
    pub build_metadata: Option<String>,
}

impl SemVer {
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some()
    }

    /// Canonical `RawVersion` string: `major.minor.patch`, with no prerelease
    /// or build-metadata suffix.
    pub fn raw_version_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }

    /// Canonical `Version` string: `major.minor.patch` plus the optional
    /// `-prerelease` and `+build-metadata` suffixes. This is the single source
    /// of truth for deriving the `Version` template var from a parsed tag, used
    /// by both [`Context::populate_git_vars`](crate::context::Context::populate_git_vars)
    /// and the build stage's per-crate re-scoping so the two never drift.
    pub fn version_string(&self) -> String {
        let mut version = self.raw_version_string();
        if let Some(ref pre) = self.prerelease {
            version.push('-');
            version.push_str(pre);
        }
        if let Some(ref meta) = self.build_metadata {
            version.push('+');
            version.push_str(meta);
        }
        version
    }
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.major == other.major
            && self.minor == other.minor
            && self.patch == other.patch
            && self.prerelease == other.prerelease
    }
}

impl Eq for SemVer {}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then(match (&self.prerelease, &other.prerelease) {
                (Some(_), None) => std::cmp::Ordering::Less, // prerelease < release
                (None, Some(_)) => std::cmp::Ordering::Greater, // release > prerelease
                (Some(a), Some(b)) => compare_prerelease(a, b),
                (None, None) => std::cmp::Ordering::Equal,
            })
    }
}

/// Compare two prerelease strings per SemVer 2.0.0 section 11.
///
/// Dot-separated identifiers are compared individually: numeric identifiers are
/// compared as integers, alphanumeric identifiers are compared lexicographically,
/// and numeric identifiers always have lower precedence than alphanumeric ones.
/// A shorter set of identifiers has lower precedence when all preceding
/// identifiers are equal.
pub(super) fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let a_ids: Vec<&str> = a.split('.').collect();
    let b_ids: Vec<&str> = b.split('.').collect();

    for (ai, bi) in a_ids.iter().zip(b_ids.iter()) {
        let ord = match (ai.parse::<u64>(), bi.parse::<u64>()) {
            (Ok(an), Ok(bn)) => an.cmp(&bn), // both numeric: compare as integers
            (Ok(_), Err(_)) => Ordering::Less, // numeric < alphanumeric
            (Err(_), Ok(_)) => Ordering::Greater, // alphanumeric > numeric
            (Err(_), Err(_)) => ai.cmp(bi),  // both alpha: lexicographic
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // Shorter set has lower precedence
    a_ids.len().cmp(&b_ids.len())
}

/// Compiled once and reused across all calls to [`parse_semver`].
///
/// Captures: 1=major, 2=minor, 3=patch, 4=prerelease (optional), 5=build metadata (optional).
/// Prerelease is after `-` but before `+`. Build metadata is after `+`.
static SEMVER_RE: LazyLock<Regex> =
    LazyLock::new(|| crate::util::static_regex(r"^v?(\d+)\.(\d+)\.(\d+)(?:-([^+]+))?(?:\+(.+))?$"));

/// Parse a strict semver version from a string like "v1.2.3", "1.2.3", "v1.0.0-rc.1",
/// "v1.0.0+build.42", or "v1.0.0-rc.1+build.42".
///
/// The string must start with an optional `v` prefix followed by the version.
/// For prefixed tags like "cfgd-core-v2.1.0", use [`parse_semver_tag`] instead.
pub fn parse_semver(tag: &str) -> Result<SemVer> {
    let caps = SEMVER_RE
        .captures(tag)
        .ok_or_else(|| anyhow::anyhow!("not a valid semver tag: {}", tag))?;
    Ok(SemVer {
        major: caps[1].parse()?,
        minor: caps[2].parse()?,
        patch: caps[3].parse()?,
        prerelease: caps.get(4).map(|m| m.as_str().to_string()),
        build_metadata: caps.get(5).map(|m| m.as_str().to_string()),
    })
}

/// Parse a semver version from a prefixed tag string.
///
/// Strips everything up to and including the last `-` or `_` before the version
/// portion, then delegates to [`parse_semver`]. Handles tags like
/// "cfgd-core-v2.1.0", "my_project-v1.0.0-rc.1", or plain "v1.2.3".
/// Canonical `Version` string a release tag stamps, whatever its family
/// prefix (`v1.2.3`, `crd-v1.2.3`, `sub/v1.2.3` → `1.2.3`). `None` for an
/// empty tag (no previous release) or a tag that does not parse as a
/// semver tag. This is the one tag→version derivation shared by every
/// consumer (version rewrites, burn-evidence filtering) so their
/// semantics cannot drift.
pub fn version_from_tag(tag: &str) -> Option<String> {
    if tag.is_empty() {
        return None;
    }
    parse_semver_tag(tag).ok().map(|sv| sv.version_string())
}

/// Split a release tag into its family prefix and the parsed version it
/// stamps: `crd-v0.5.0` → (`"crd-v"`, 0.5.0), `v1.2.3` → (`"v"`, 1.2.3),
/// `sub/v1.2.3-rc.1` → (`"sub/v"`, 1.2.3-rc.1). Two tags with equal
/// prefixes belong to the same tag family (the same `tag_template`
/// track). `None` when no semver version can be located in the tag.
pub fn split_tag_family(tag: &str) -> Option<(&str, SemVer)> {
    static FAMILY_RE: LazyLock<Regex> = LazyLock::new(|| {
        crate::util::static_regex(r"^((?:|.*[-_/])v?)(\d+\.\d+\.\d+(?:-[^+]+)?(?:\+.+)?)$")
    });
    let caps = FAMILY_RE.captures(tag)?;
    let prefix_len = caps.get(1)?.as_str().len();
    let sv = parse_semver(caps.get(2)?.as_str()).ok()?;
    Some((&tag[..prefix_len], sv))
}

pub fn parse_semver_tag(tag: &str) -> Result<SemVer> {
    // Try strict parse first (handles "v1.2.3" and "1.2.3")
    if let Ok(sv) = parse_semver(tag) {
        return Ok(sv);
    }
    // Find the version portion: look for `v?\d+.\d+.\d+` after a separator
    static PREFIX_RE: LazyLock<Regex> =
        LazyLock::new(|| crate::util::static_regex(r"[-_/](v?\d+\.\d+\.\d+(?:-[^+]+)?(?:\+.+)?)$"));
    if let Some(caps) = PREFIX_RE.captures(tag) {
        return parse_semver(&caps[1]);
    }
    anyhow::bail!("not a valid semver tag: {}", tag)
}
