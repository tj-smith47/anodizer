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
///
/// # Why not `util::config::resolve_token`?
///
/// The sibling `resolve_token` helper covers a *narrower* problem:
/// "given a `Context` and a publisher-specific env-var hint, produce
/// the resolved token string (with fallbacks)". This helper is broader
/// — it answers "is the env var named by an arbitrary scope label
/// set?" for any rollback-scope label the publisher returns. Routing
/// through `resolve_token` would force the rollback path to thread
/// `Context` everywhere just to do an env-var presence check, and
/// would still need a special case for non-`GITHUB_TOKEN` labels
/// (e.g. `KREW_INDEX_TOKEN write`). The two helpers share the
/// `ANODIZER_GITHUB_TOKEN` alias by design; the duplication is
/// bounded to that one block.
///
/// Delegates to [`scope_available_with_env`] against
/// [`anodizer_core::ProcessEnvSource`]; rollback / preflight call
/// sites that already hold a [`Context`] route through
/// [`scope_available_with_env`] directly so the lookup honors any
/// injected [`MapEnvSource`](anodizer_core::MapEnvSource).
#[allow(dead_code)]
pub(crate) fn scope_available(label: &str) -> bool {
    scope_available_with_env(label, &anodizer_core::ProcessEnvSource)
}

/// Env-injectable form of [`scope_available`]. Production call sites
/// in `rollback.rs` / `rollback_only.rs` / `preflight.rs` thread the
/// active [`Context`]'s env source through here so a unit test can
/// drive the available/unavailable branches without mutating the
/// process env.
pub(crate) fn scope_available_with_env<E: anodizer_core::EnvSource + ?Sized>(
    label: &str,
    env: &E,
) -> bool {
    let env_var = label.split_once(' ').map(|(v, _)| v).unwrap_or(label);
    if env.var(env_var).map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    if env_var == "GITHUB_TOKEN"
        && env
            .var("ANODIZER_GITHUB_TOKEN")
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
    use anodizer_core::MapEnvSource;

    #[test]
    fn scope_available_returns_true_when_env_set() {
        let env = MapEnvSource::new().with("SCOPE_TEST_TOKEN_SET", "xyz");
        assert!(scope_available_with_env("SCOPE_TEST_TOKEN_SET write", &env));
        // No trailing scope text is fine — bare name also matches.
        assert!(scope_available_with_env("SCOPE_TEST_TOKEN_SET", &env));
    }

    #[test]
    fn scope_available_returns_false_when_env_unset() {
        let env = MapEnvSource::new();
        assert!(!scope_available_with_env(
            "SCOPE_TEST_TOKEN_UNSET write",
            &env
        ));
    }

    #[test]
    fn scope_available_returns_false_when_env_empty() {
        let env = MapEnvSource::new().with("SCOPE_TEST_TOKEN_EMPTY", "");
        assert!(!scope_available_with_env(
            "SCOPE_TEST_TOKEN_EMPTY write",
            &env
        ));
    }

    #[test]
    fn scope_available_honors_anodizer_github_token_fallback() {
        let env = MapEnvSource::new().with("ANODIZER_GITHUB_TOKEN", "yyy");
        assert!(scope_available_with_env(
            "GITHUB_TOKEN contents:write",
            &env
        ));
    }

    #[test]
    fn scope_available_anodizer_fallback_only_applies_to_github_token() {
        // The ANODIZER_ fallback is hard-coded to GITHUB_TOKEN; sibling
        // vars do NOT get the same alias treatment.
        let env = MapEnvSource::new().with("ANODIZER_OTHER_TOKEN", "yyy");
        assert!(!scope_available_with_env("OTHER_TOKEN write", &env));
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
