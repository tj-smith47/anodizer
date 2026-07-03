//! Conventional-Commits → release-level classification.
//!
//! The single classifier behind both release-inference surfaces: the `tag`
//! command's auto-bump (which layers `#token` / `#none` / `default_bump`
//! precedence on top) and the `bump` command's per-crate inference. Both
//! must answer "what does this commit imply for the next version?"
//! identically, or `bump --dry-run` previews a different release than
//! auto-tag actually cuts.

/// Release level implied by a single Conventional-Commits message.
///
/// Variants are ordered `Patch < Minor < Major` so a range of commits can be
/// folded with `max()` — the strongest signal in the range wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConventionalLevel {
    /// `fix:` / `perf:` / `revert:` (and scoped variants).
    Patch,
    /// `feat:` (and scoped variants).
    Minor,
    /// A line containing `BREAKING CHANGE` / `BREAKING-CHANGE`, or a
    /// `<type>!:` breaking shorthand.
    Major,
}

/// Classify one commit message (subject + body) against the
/// Conventional-Commits release rules.
///
/// Returns `None` for non-release-worthy commits: `chore:` / `docs:` /
/// `style:` / `refactor:` / `test:` / `build:` / `ci:` typed subjects and
/// unstructured messages.
///
/// Rules:
/// - `BREAKING CHANGE` / `BREAKING-CHANGE` anywhere in the message → major.
///   Matched as a bare substring (no trailing colon required) so a footer
///   like `BREAKING CHANGE removed the old endpoint` still majors.
/// - `<type>!:` / `<type>(scope)!:` breaking shorthand → major, for ANY type
///   (`refactor!:` is still a breaking change).
/// - `feat:` / `feat(scope):` → minor.
/// - `fix:` / `perf:` / `revert:` (and scoped variants) → patch.
pub fn classify_commit(msg: &str) -> Option<ConventionalLevel> {
    if msg.contains("BREAKING CHANGE") || msg.contains("BREAKING-CHANGE") {
        return Some(ConventionalLevel::Major);
    }
    // Only the subject line carries the type prefix.
    let subject = msg.lines().next().unwrap_or("").trim_start();
    let (ty, _rest) = subject.split_once(':')?;
    // Strip a `(scope)` suffix and capture the post-scope `!` breaking marker.
    let (head, marker) = ty.split_once('(').map_or((ty, ""), |(h, scope_rest)| {
        // scope_rest is like `scope)!` or `scope)` — extract the post-`)` part.
        let after_scope = scope_rest.split_once(')').map_or("", |x| x.1);
        (h, after_scope)
    });
    if marker.starts_with('!') || ty.ends_with('!') {
        return Some(ConventionalLevel::Major);
    }
    match head.trim() {
        "feat" => Some(ConventionalLevel::Minor),
        "fix" | "perf" | "revert" => Some(ConventionalLevel::Patch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaking_change_footer_is_major() {
        assert_eq!(
            classify_commit("feat(api): new endpoint\n\nBREAKING CHANGE: old endpoint removed"),
            Some(ConventionalLevel::Major)
        );
    }

    #[test]
    fn breaking_change_without_colon_is_major() {
        assert_eq!(
            classify_commit("feat: x\n\nBREAKING CHANGE removed the old endpoint"),
            Some(ConventionalLevel::Major)
        );
        assert_eq!(
            classify_commit("fix: y\n\nBREAKING-CHANGE dropped the flag"),
            Some(ConventionalLevel::Major)
        );
    }

    #[test]
    fn bang_shorthand_is_major_for_any_type() {
        assert_eq!(
            classify_commit("feat!: drop legacy auth"),
            Some(ConventionalLevel::Major)
        );
        assert_eq!(
            classify_commit("feat(core)!: rewrite pipeline"),
            Some(ConventionalLevel::Major)
        );
        assert_eq!(
            classify_commit("refactor!: drop the shim"),
            Some(ConventionalLevel::Major)
        );
    }

    #[test]
    fn feat_is_minor() {
        assert_eq!(
            classify_commit("feat: new stage"),
            Some(ConventionalLevel::Minor)
        );
        assert_eq!(
            classify_commit("feat(build): add cache key"),
            Some(ConventionalLevel::Minor)
        );
    }

    #[test]
    fn fix_perf_and_revert_are_patch() {
        assert_eq!(classify_commit("fix: race"), Some(ConventionalLevel::Patch));
        assert_eq!(
            classify_commit("perf: faster loop"),
            Some(ConventionalLevel::Patch)
        );
        assert_eq!(
            classify_commit("revert: undo broken feature"),
            Some(ConventionalLevel::Patch)
        );
    }

    #[test]
    fn non_release_worthy_types_are_none() {
        assert_eq!(classify_commit("chore: update deps"), None);
        assert_eq!(classify_commit("docs: fix link"), None);
        assert_eq!(classify_commit("refactor: rename"), None);
        assert_eq!(classify_commit("ci: pin runner"), None);
    }

    #[test]
    fn unstructured_message_is_none() {
        assert_eq!(classify_commit("random subject"), None);
    }

    #[test]
    fn levels_order_by_strength_for_max_folds() {
        assert!(ConventionalLevel::Patch < ConventionalLevel::Minor);
        assert!(ConventionalLevel::Minor < ConventionalLevel::Major);
    }
}
