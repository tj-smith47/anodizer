//! Shared scope-label availability check for the rollback dispatch
//! and preflight paths.
//!
//! Parses a rollback-scope label like `"GITHUB_TOKEN delete_repo"` or
//! `"CARGO_REGISTRY_TOKEN yank"` and returns `true` when the env var
//! named in the first whitespace-separated token is set to a non-empty
//! value in the process environment. The trailing scope description
//! after the space is informational only — we cannot verify scope
//! strings against the actual token's permissions without an API
//! round-trip.
//!
//! Special-case: `GITHUB_TOKEN` also accepts the anodize-specific
//! override `ANODIZER_GITHUB_TOKEN` (the same fallback pattern that
//! publish / rollback paths use for GitHub-credentialed publishers).
//!
//! # Why a shared helper
//!
//! `rollback::scope_available` and `preflight::scope_label_is_available`
//! were character-for-character identical (the latter had an unused
//! `_ctx` parameter that gave the illusion of a difference). Two
//! definitions of the same env-lookup is one too many: a future
//! scope-label scheme change has to be
//! mirrored across both call sites or the rollback path silently
//! diverges from preflight.
//!
//! Lives in `stage-publish` (not `core`) because the label format is
//! a stage-publish-internal detail: the `Publisher::rollback_scope_needed`
//! trait method returns the label string, but every consumer of that
//! string is inside `stage-publish` (rollback dispatch, preflight, the
//! `rollback_only` replay path).

/// Format a uniform "scope unavailable" warn line for the three
/// call sites that consume `Publisher::rollback_scope_needed`
/// (`rollback::run`, `rollback_only::run_with_publishers`,
/// `preflight::run_publisher_preflight_extension`). Audit ref:
/// B6 review M-7 + M-8 — three character-drifted messages
/// collapsed into one source of truth.
///
/// `prefix` identifies the calling subsystem (`"rollback"`,
/// `"rollback-only"`, `"preflight"`); `publisher` and `label` are
/// the publisher name and the raw scope label string.
///
/// The output explicitly names "env scope" so the operator sees
/// the remedy is `export <VAR>=...`, not some other class of
/// credential.
pub(crate) fn warn_scope_unavailable_msg(prefix: &str, publisher: &str, label: &str) -> String {
    format!(
        "{prefix}: '{publisher}' skipped — env scope '{label}' unavailable \
         (set the env var to enable rollback)"
    )
}

/// Returns `true` when the env var named by the first whitespace-
/// separated token of `label` is set to a non-empty value, OR when
/// the var is `GITHUB_TOKEN` and `ANODIZER_GITHUB_TOKEN` is set.
///
/// See module docs for the label-format rationale.
pub(crate) fn scope_available(label: &str) -> bool {
    let env_var = label.split_once(' ').map(|(v, _)| v).unwrap_or(label);
    if std::env::var(env_var)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if env_var == "GITHUB_TOKEN"
        && std::env::var("ANODIZER_GITHUB_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Helper to atomically swap an env var for the duration of a test
    /// closure, then restore the prior value. Avoids cross-test bleed
    /// when serial_test ordering doesn't apply (within a single test,
    /// multiple set/unset pairs).
    fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let prior = std::env::var(key).ok();
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        // SAFETY: env mutation is single-threaded within a serial group.
        unsafe {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    #[serial(scope_env)]
    fn scope_available_returns_true_when_env_set() {
        with_env("SCOPE_TEST_TOKEN_SET", Some("xyz"), || {
            assert!(scope_available("SCOPE_TEST_TOKEN_SET write"));
            // No trailing scope text is fine — bare name also matches.
            assert!(scope_available("SCOPE_TEST_TOKEN_SET"));
        });
    }

    #[test]
    #[serial(scope_env)]
    fn scope_available_returns_false_when_env_unset() {
        with_env("SCOPE_TEST_TOKEN_UNSET", None, || {
            assert!(!scope_available("SCOPE_TEST_TOKEN_UNSET write"));
        });
    }

    #[test]
    #[serial(scope_env)]
    fn scope_available_returns_false_when_env_empty() {
        with_env("SCOPE_TEST_TOKEN_EMPTY", Some(""), || {
            assert!(!scope_available("SCOPE_TEST_TOKEN_EMPTY write"));
        });
    }

    #[test]
    #[serial(scope_env)]
    fn scope_available_honors_anodizer_github_token_fallback() {
        with_env("GITHUB_TOKEN", None, || {
            with_env("ANODIZER_GITHUB_TOKEN", Some("yyy"), || {
                assert!(scope_available("GITHUB_TOKEN contents:write"));
            });
        });
    }

    #[test]
    #[serial(scope_env)]
    fn scope_available_anodizer_fallback_only_applies_to_github_token() {
        with_env("OTHER_TOKEN", None, || {
            with_env("ANODIZER_OTHER_TOKEN", Some("yyy"), || {
                // The ANODIZER_ fallback is hard-coded to GITHUB_TOKEN;
                // sibling vars do NOT get the same alias treatment.
                assert!(!scope_available("OTHER_TOKEN write"));
            });
        });
    }

    // ---- warn_scope_unavailable_msg --------------------------------------

    #[test]
    fn warn_msg_includes_prefix_publisher_label_and_remedy() {
        let msg = warn_scope_unavailable_msg("rollback", "homebrew", "GITHUB_TOKEN contents:write");
        assert!(msg.contains("rollback:"), "missing prefix: {msg}");
        assert!(msg.contains("'homebrew'"), "missing publisher: {msg}");
        assert!(
            msg.contains("'GITHUB_TOKEN contents:write'"),
            "missing label: {msg}"
        );
        assert!(
            msg.contains("set the env var"),
            "missing remedy hint: {msg}",
        );
    }

    #[test]
    fn warn_msg_uses_supplied_prefix_verbatim() {
        // Regression guard: every call site sets its own prefix. The
        // helper must NOT lowercase / dash-normalize / otherwise
        // mangle it.
        let r = warn_scope_unavailable_msg("rollback-only", "p", "X");
        assert!(r.starts_with("rollback-only:"), "got {r}");
        let p = warn_scope_unavailable_msg("preflight", "p", "X");
        assert!(p.starts_with("preflight:"), "got {p}");
    }
}
