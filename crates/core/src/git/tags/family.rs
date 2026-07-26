//! Tag naming and family scoping.
//!
//! The vocabulary every other submodule filters with: the version-placeholder
//! forms a `tag_template` may use, the `ignore_tags` / `ignore_tag_prefixes` /
//! nightly exclusions, and [`TagFamilyScope`] — the release track a template
//! mints, which scopes both the git-side (`--match=<glob>`) and Rust-side
//! (list + parse) tag searches so a multi-track workspace never answers a
//! `core-v` question with a `v` tag.

use crate::config::GitConfig;
use crate::git::semver::{SemVer, parse_semver_tag};
use crate::template::TemplateVars;

/// Render ignore patterns (both `ignore_tags` and `ignore_tag_prefixes`) through
/// the template engine when `template_vars` is provided.
///
/// Returns two vecs: `(rendered_ignore_tags, rendered_ignore_tag_prefixes)`.
/// When `vars` is `None`, patterns are returned as-is (unrendered).
pub fn render_ignore_patterns(
    git_config: Option<&GitConfig>,
    vars: Option<&TemplateVars>,
) -> (Vec<String>, Vec<String>) {
    let rendered_tags: Vec<String> = git_config
        .and_then(|gc| gc.ignore_tags.as_ref())
        .map(|v| {
            v.iter()
                .map(|s| {
                    if let Some(tv) = vars {
                        crate::template::render(s, tv).unwrap_or_else(|_| s.clone())
                    } else {
                        s.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let rendered_prefixes: Vec<String> = git_config
        .and_then(|gc| gc.ignore_tag_prefixes.as_ref())
        .map(|v| {
            v.iter()
                .map(|s| {
                    if let Some(tv) = vars {
                        crate::template::render(s, tv).unwrap_or_else(|_| s.clone())
                    } else {
                        s.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    (rendered_tags, rendered_prefixes)
}

/// Whether `tag` carries anodizer's own nightly marker: the literal tag
/// `nightly` (the default `nightly.tag_name`), or a `nightly` token delimited
/// by `-` / `.` / `+` inside the version portion — the shape every supported
/// `nightly.version_template` produces (default
/// `…-{{ ShortCommit }}-nightly`, nushell-style `…-nightly.{{ NightlyBuild }}+…`).
///
/// Nightly tags are an implementation detail of the nightly pipeline, never a
/// user release signal, so stable-release surfaces (crate selection from
/// tags-at-HEAD, latest-tag / previous-tag resolution) exclude them
/// UNCONDITIONALLY — a stranded tag from a failed nightly run must not poison
/// the next stable release regardless of `git.ignore_tags` config. A leading
/// `nightly` with no preceding delimiter (e.g. a crate genuinely named
/// `nightly-tools`, tag `nightly-tools-v1.0.0`) is NOT matched.
pub fn is_nightly_tag(tag: &str) -> bool {
    if tag == "nightly" {
        return true;
    }
    // Only the VERSION portion may carry the nightly marker: a crate honestly
    // named `app-nightly` must keep its stable `app-nightly-v1.0.0` tags. The
    // version portion starts at the last `v<digit>` (per-crate tags are
    // `<prefix>v<version>`); a tag with no `v<digit>` (bare-version templates
    // like nushell's `0.107.0-nightly.4+sha`) is scanned whole.
    let scan_from = tag
        .char_indices()
        .rfind(|&(i, c)| c == 'v' && tag[i + 1..].starts_with(|n: char| n.is_ascii_digit()))
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    let scan = &tag[scan_from..];
    const DELIMS: [char; 3] = ['-', '.', '+'];
    let bytes = scan.as_bytes();
    let mut from = 0;
    while let Some(pos) = scan[from..].find("nightly") {
        let start = from + pos;
        let end = start + "nightly".len();
        let preceded = start > 0 && DELIMS.contains(&(bytes[start - 1] as char));
        let followed = end == scan.len() || DELIMS.contains(&(bytes[end] as char));
        if preceded && followed {
            return true;
        }
        from = end;
    }
    false
}

/// Built-in `git describe --exclude` globs equivalent to [`is_nightly_tag`]
/// for the describe-based previous-tag path. Anchored the same way: the
/// `nightly` token must follow a `v<digit>` version start (or begin a
/// bare-version tag with a leading digit), so a crate named `app-nightly`
/// keeps its stable `app-nightly-v1.0.0` tags visible to describe.
const NIGHTLY_EXCLUDE_GLOBS: [&str; 5] = [
    "nightly",
    "*v[0-9]*[-.+]nightly",
    "*v[0-9]*[-.+]nightly[-.+]*",
    "[0-9]*[-.+]nightly",
    "[0-9]*[-.+]nightly[-.+]*",
];

/// [`NIGHTLY_EXCLUDE_GLOBS`] rendered as `--exclude=<glob>` arguments for any
/// `git describe` invocation that must not resolve to a nightly tag.
pub(crate) fn nightly_exclude_describe_args() -> Vec<String> {
    NIGHTLY_EXCLUDE_GLOBS
        .iter()
        .map(|pat| format!("--exclude={}", pat))
        .collect()
}

/// Filter an arbitrary tag list through the user's `ignore_tags` (glob) /
/// `ignore_tag_prefixes` (starts-with) config plus the unconditional
/// [`is_nightly_tag`] exclusion — the same semantics
/// [`find_latest_tag_matching_in`](crate::git::find_latest_tag_matching_in)
/// applies to its candidate set, packaged for callers that obtain tags
/// elsewhere (e.g. tags-at-HEAD crate selection).
/// Ignore patterns are template-rendered when `vars` is provided.
pub fn filter_ignored_tags(
    tags: &[String],
    git_config: Option<&GitConfig>,
    vars: Option<&TemplateVars>,
) -> Vec<String> {
    let (rendered_ignore_tags, rendered_ignore_prefixes) = render_ignore_patterns(git_config, vars);
    let ignore_tag_globs: Vec<glob::Pattern> = rendered_ignore_tags
        .iter()
        .filter_map(|pat| glob::Pattern::new(pat).ok())
        .collect();
    tags.iter()
        .filter(|t| !is_nightly_tag(t))
        .filter(|t| !ignore_tag_globs.iter().any(|g| g.matches(t)))
        .filter(|t| {
            !rendered_ignore_prefixes
                .iter()
                .any(|p| !p.is_empty() && t.starts_with(p.as_str()))
        })
        .cloned()
        .collect()
}

/// The four accepted placeholder forms for the version variable in tag templates.
pub(super) const VERSION_PLACEHOLDERS: &[&str] = &[
    "{{ .Version }}",
    "{{.Version}}",
    "{{ Version }}",
    "{{Version}}",
];

/// Check whether a tag template string contains any recognised version placeholder.
pub fn has_version_placeholder(template: &str) -> bool {
    VERSION_PLACEHOLDERS.iter().any(|p| template.contains(p))
}

/// Borrowed form of [`extract_tag_prefix`]: the slice of `template` preceding
/// the first recognised version placeholder.
fn tag_prefix_slice(template: &str) -> Option<&str> {
    VERSION_PLACEHOLDERS
        .iter()
        .find_map(|ph| template.find(ph).map(|idx| &template[..idx]))
}

/// Extract the prefix portion of a tag template by locating the version placeholder.
///
/// Returns the substring before the first recognised placeholder, or `None` if no
/// placeholder is found.
///
/// A template that starts with the placeholder (`{{ Version }}`) yields
/// `Some("")` — a prefix that scopes nothing. Callers scoping a tag *family*
/// must route through
/// [`find_previous_tag_in_family`](crate::git::find_previous_tag_in_family)
/// rather than feeding that empty string to a prefix filter.
pub fn extract_tag_prefix(template: &str) -> Option<String> {
    tag_prefix_slice(template).map(str::to_string)
}

/// The set of tags one `tag_template` mints, as a filter both the git-side
/// (`--match=<glob>`) and the Rust-side (list + parse) previous-tag paths can
/// apply.
///
/// A literal prefix covers every ordinary template (`v`, `core-v`,
/// `subproject1/`). It cannot express the bare `{{ Version }}` family: its
/// extracted prefix is the empty string, which matches every tag in the
/// repository — including a sibling track's `core-v0.6.0` — and so would
/// silently un-scope the search it was meant to narrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TagFamilyScope<'a> {
    /// Tags starting with a literal prefix.
    Prefix(&'a str),
    /// Tags that are a bare version (`0.6.0`), minted by a template whose
    /// version placeholder sits at position zero.
    BareVersion,
}

impl TagFamilyScope<'_> {
    /// The glob for `git describe --match=<glob>`.
    pub(super) fn describe_glob(&self) -> String {
        match self {
            Self::Prefix(p) => format!("{p}*"),
            Self::BareVersion => "[0-9]*".to_string(),
        }
    }

    /// Whether `tag` belongs to this family.
    fn contains(&self, tag: &str) -> bool {
        match self {
            Self::Prefix(p) => tag.starts_with(p),
            Self::BareVersion => tag.starts_with(|c: char| c.is_ascii_digit()),
        }
    }

    /// The tag with this family's prefix removed, ready for SemVer parsing.
    pub(super) fn strip<'t>(&self, tag: &'t str) -> &'t str {
        match self {
            Self::Prefix(p) => strip_monorepo_prefix(tag, p),
            Self::BareVersion => tag,
        }
    }
}

/// The glob describing the tag family `tag_template` mints — literally the
/// filter [`find_previous_tag_in_family`](crate::git::find_previous_tag_in_family)
/// applies, so a diagnostic quoting it cannot drift from the search that was
/// actually performed. `None` when
/// nothing scopes the search.
///
/// # Examples
/// ```
/// # use anodizer_core::git::tag_family_glob;
/// assert_eq!(tag_family_glob("v{{ .Version }}", None).as_deref(), Some("v*"));
/// assert_eq!(tag_family_glob("{{ Version }}", None).as_deref(), Some("[0-9]*"));
/// assert_eq!(tag_family_glob("nightly", None), None);
/// ```
pub fn tag_family_glob(tag_template: &str, monorepo_prefix: Option<&str>) -> Option<String> {
    tag_family_scope(tag_template, monorepo_prefix).map(|s| s.describe_glob())
}

/// Resolve the family a crate's `tag_template` mints, falling back to the
/// monorepo namespace when the template carries no usable scope of its own.
pub(super) fn tag_family_scope<'a>(
    tag_template: &'a str,
    monorepo_prefix: Option<&'a str>,
) -> Option<TagFamilyScope<'a>> {
    match tag_prefix_slice(tag_template) {
        Some(p) if !p.is_empty() => Some(TagFamilyScope::Prefix(p)),
        // A bare `{{ Version }}` under a monorepo namespace still lives inside
        // that namespace (`subproject1/0.6.0`), so the namespace is the family.
        Some(_) => match monorepo_prefix.filter(|p| !p.is_empty()) {
            Some(p) => Some(TagFamilyScope::Prefix(p)),
            None => Some(TagFamilyScope::BareVersion),
        },
        None => monorepo_prefix
            .filter(|p| !p.is_empty())
            .map(TagFamilyScope::Prefix),
    }
}

/// The tag-family prefix used for a crate: the prefix extracted from its
/// `tag_template`, falling back to the `<name>-v` convention when the
/// template is empty or carries no recognised version placeholder.
///
/// Every surface that scans or mints per-crate tags (`tag`, `bump` range
/// inference, `changelog` tag-owner resolution and crate selection) must
/// resolve the SAME family from the same inputs: a drifted fallback makes
/// the last-tag probe come up empty and silently widens the commit range
/// to full history.
pub fn per_crate_tag_prefix(name: &str, tag_template: &str) -> String {
    extract_tag_prefix(tag_template).unwrap_or_else(|| format!("{name}-v"))
}

/// Strip a monorepo tag prefix from a tag string.
///
/// If `tag` starts with `prefix`, returns the remainder; otherwise returns
/// the original tag unchanged.
///
/// # Examples
/// ```
/// # use anodizer_core::git::strip_monorepo_prefix;
/// assert_eq!(strip_monorepo_prefix("subproject1/v1.2.3", "subproject1/"), "v1.2.3");
/// assert_eq!(strip_monorepo_prefix("v1.2.3", "subproject1/"), "v1.2.3");
/// ```
pub fn strip_monorepo_prefix<'a>(tag: &'a str, prefix: &str) -> &'a str {
    tag.strip_prefix(prefix).unwrap_or(tag)
}

/// Which form of a tag the `ignore_tags` / `ignore_tag_prefixes` filters match
/// against in a monorepo context.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IgnoreMatchTarget {
    /// Match ignores against the monorepo-stripped tag (so user patterns like
    /// `v*-rc*` work without the prefix). Used by
    /// [`find_latest_tag_matching_in`](crate::git::find_latest_tag_matching_in).
    Stripped,
    /// Match ignores against the full tag name — identical to the legacy
    /// `git describe --exclude=<pat>` path regardless of `monorepo_prefix`.
    /// Used by [`super::previous`]'s `smartsemver_previous_tag_in`.
    Full,
}

/// Parse `tags_output` lines into `(SemVer, tag)` pairs, applying the shared
/// family membership filter, the `ignore_tags` glob filter, the
/// `ignore_tag_prefixes` starts-with filter, and SemVer parsing (stripping the
/// family prefix before parsing). Unsorted — the caller picks ascending vs
/// descending and may layer additional per-site filters (regex match,
/// current-tag exclusion, prerelease skip).
///
/// `ignore_target` selects whether the ignore filters see the stripped or full
/// tag. `skip_empty_ignore_prefix` controls whether an empty rendered
/// `ignore_tag_prefixes` entry is ignored (`true`) or allowed to match every
/// tag (`false`) — preserving each call site's historical behavior.
pub(super) fn semver_pairs_filtered(
    tags_output: &str,
    scope: Option<TagFamilyScope<'_>>,
    ignore_tag_globs: &[glob::Pattern],
    rendered_ignore_prefixes: &[String],
    ignore_target: IgnoreMatchTarget,
    skip_empty_ignore_prefix: bool,
) -> Vec<(SemVer, String)> {
    let strip = |t: &str| -> String { scope.map(|s| s.strip(t)).unwrap_or(t).to_string() };
    let ignore_view = |t: &str| -> String {
        match ignore_target {
            IgnoreMatchTarget::Stripped => strip(t),
            IgnoreMatchTarget::Full => t.to_string(),
        }
    };
    tags_output
        .lines()
        .filter(|t| !is_nightly_tag(t))
        .filter(|t| scope.map(|s| s.contains(t)).unwrap_or(true))
        .filter(|t| {
            let view = ignore_view(t);
            !ignore_tag_globs.iter().any(|g| g.matches(&view))
        })
        .filter(|t| {
            let view = ignore_view(t);
            !rendered_ignore_prefixes.iter().any(|p| {
                (!skip_empty_ignore_prefix || !p.is_empty()) && view.starts_with(p.as_str())
            })
        })
        .filter_map(|t| parse_semver_tag(&strip(t)).ok().map(|v| (v, t.to_string())))
        .collect()
}
