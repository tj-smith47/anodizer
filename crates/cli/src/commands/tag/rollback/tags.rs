use super::types::Scope;
use anodizer_core::git;
use regex::Regex;
use std::sync::LazyLock;

/// Strict semver-ish per-crate tag pattern: `<crate>-v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`.
/// The crate-name portion accepts ASCII letters, `_` and `-` as the
/// first char (cargo crate names must start with a letter — digits are
/// rejected), then letters/digits/`_`/`-` for the remainder; the
/// suffix is then asserted to be anodize's `v<semver>` form so a tag like
/// `foo-bar` (no `-v` suffix) doesn't accidentally match.
///
/// Compiled once at first use (the pattern is a compile-time literal) so
/// the classifier doesn't recompile it per tag — same caching idea as
/// `is_branchlike` in `core/git/commits.rs`.
///
/// Drift-risk pair with `core::git::is_branchlike`: that predicate matches
/// the same two anodize tag shapes but with deliberately looser, prefix-only
/// regexes (it answers "is this NOT a tag?" for branch fallback, so it must
/// not over-strict). These rollback patterns are fully anchored and strict
/// on purpose. Keep the two shape definitions in sync when the tag grammar
/// changes — they are intentionally separate, not accidentally duplicated.
pub(super) static PER_CRATE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_-]*-v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$")
        .expect("static regex compiles")
});

/// Lockstep tag pattern: `v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`. Compiled
/// once at first use (see [`PER_CRATE_TAG_RE`]).
pub(super) static LOCKSTEP_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$")
        .expect("static regex compiles")
});

/// Classification used to filter tags against the requested `--scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TagKind {
    Lockstep,
    PerCrate,
}

/// Classify a tag against anodize's naming conventions. Returns `None`
/// when the tag doesn't match either shape (in which case the rollback
/// command leaves it alone).
pub(super) fn classify_tag(tag: &str) -> Option<TagKind> {
    // Lockstep first — `vX.Y.Z` would also fail the per-crate regex's
    // `<crate>-` prefix requirement, but the explicit ordering keeps the
    // intent obvious to a reader.
    if LOCKSTEP_TAG_RE.is_match(tag) {
        Some(TagKind::Lockstep)
    } else if PER_CRATE_TAG_RE.is_match(tag) {
        Some(TagKind::PerCrate)
    } else {
        None
    }
}

/// Apply the `--scope` filter on top of the classification.
pub(super) fn scope_includes(scope: Scope, kind: TagKind) -> bool {
    matches!(
        (scope, kind),
        (Scope::All, _)
            | (Scope::Lockstep, TagKind::Lockstep)
            | (Scope::PerCrate, TagKind::PerCrate)
    )
}

/// Build the rollback commit subject line. The tags list goes in the
/// body so a long per-crate batch doesn't blow past 72 chars. When
/// `dry_run` is true, the tag list is prefixed with "WOULD be" to
/// signal that the preview commit message describes pending (not
/// actually applied) state — otherwise a `--dry-run` printout reads
/// identically to a real-run one and fools the operator.
pub(super) fn build_revert_message(
    target_sha: &str,
    deleted_tags: &[String],
    dry_run: bool,
) -> String {
    let primary = deleted_tags
        .iter()
        .find(|t| LOCKSTEP_TAG_RE.is_match(t))
        .cloned()
        .unwrap_or_else(|| {
            deleted_tags
                .first()
                .cloned()
                .unwrap_or_else(|| "release".to_string())
        });
    let short = if target_sha.len() > 7 {
        &target_sha[..7]
    } else {
        target_sha
    };
    let mut body = format!(
        "{} {primary} [skip ci]\n\nReverts {short}.",
        rollback_subject_prefix()
    );
    if !deleted_tags.is_empty() {
        let label = if dry_run {
            "Tags that WOULD be deleted"
        } else {
            "Tags deleted"
        };
        body.push_str(&format!("\n{label}: {}", deleted_tags.join(", ")));
    }
    body
}

/// Subject prefix of anodize's own rollback commits
/// (`chore(release): rollback …`), composed from the shared
/// release-machinery prefix so the writer ([`build_revert_message`]) and
/// the safety-check matcher below can never drift apart.
pub(super) fn rollback_subject_prefix() -> String {
    format!("{}rollback", git::RELEASE_COMMIT_PREFIX)
}

/// Prefix that a plain `git revert` of an anodize release-machinery commit
/// produces (the amend-failure window, where the custom rollback subject
/// was never applied). Used by the rollback safety check to recognise its
/// own prior revert commit (so re-runs are idempotent) without absorbing
/// unrelated `Revert "<...>"` commits that GitHub's "Revert this PR"
/// button emits with arbitrary upstream subjects. Composed from the shared
/// prefix the bump/rollback writers stamp.
pub(super) static ANODIZE_REVERT_SUBJECT_PREFIX: LazyLock<String> =
    LazyLock::new(|| format!("Revert \"{}", git::RELEASE_COMMIT_PREFIX));
