//! Shared boilerplate for `Publisher` trait wrappers.
//!
//! Each top-level publisher in this crate has the same shape: a unit struct,
//! `new()` / `Default`, and a `Publisher` impl whose `name`, `group`,
//! `required`, and `rollback_scope_needed` methods are all constant returns.
//! The only per-publisher bodies are `run`, `rollback`, and `preflight`.
//!
//! The [`simple_publisher!`] macro emits the boilerplate so each publisher
//! file carries only the methods that vary. Per-publisher impls add `run`,
//! `rollback`, and `preflight` in their own `impl Publisher for X { ... }`
//! block. Rust permits the constant-returning methods to live in a separate
//! `impl Publisher` block from the per-publisher bodies — the trait is
//! satisfied across the union of all `impl Publisher for X` blocks for `X`.
//!
//! [`rollback_empty_warning_msg`] returns the exact message a publisher
//! emits when `rollback()` is invoked with no evidence to act on. Exposed
//! as a free function so unit tests can pin the wording without needing to
//! capture stderr (`eprintln!` cannot be intercepted from the same process
//! portably). Each Bundle A publisher's `rollback()` calls this helper for
//! the empty-evidence branch.
//!
//! [`is_top_level_block_configured`] is the canonical shape for the
//! `is_X_configured` predicates that walk a `Option<Vec<_>>` field on
//! [`anodizer_core::config::Config`]: a publisher counts as configured iff
//! the field is `Some` and the inner vec is non-empty. Empty-vec keeps the
//! field present in serialized config but disables the publisher, matching
//! the behavior of every `publish_to_*` body that early-returns on the
//! same condition.

/// Re-export of [`anodizer_core::rollback_empty_warning_msg`] under the
/// crate-local path. The canonical implementation lives in
/// `anodizer_core` so `stage-blob`'s `BlobPublisher` can share the exact
/// same wording — every publisher that needs an empty-evidence warn goes
/// through this single helper.
pub(crate) use anodizer_core::rollback_empty_warning_msg;

/// Canonical `is_X_configured` shape for a top-level publisher block whose
/// presence on [`anodizer_core::config::Config`] is `Option<Vec<T>>`.
///
/// True iff the field is `Some` and the inner vec is non-empty. Empty-vec
/// is treated as not-configured because every `publish_to_*` function with
/// this config shape early-returns on the same condition.
pub(crate) fn is_top_level_block_configured<T>(field: Option<&Vec<T>>) -> bool {
    field.is_some_and(|v| !v.is_empty())
}

/// Canonical wording for a Bundle B publisher's rollback failure warn line.
///
/// Bundle B rollbacks shell out to `git revert HEAD --no-edit` + `git push`.
/// The dominant failure mode operators hit is missing-credentials: `git`
/// prints `could not read Username for 'https://github.com'` and exits
/// non-zero. The operator-facing warn line needs to name (1) the publisher
/// and target, (2) the underlying error, (3) the remote URL for manual
/// cleanup, and (4) the env var the operator should set to fix it.
///
/// Hint forms:
/// - HTTPS (homebrew / scoop / nix): pass `Some("HOMEBREW_TAP_TOKEN")` so
///   the hint reads `check $HOMEBREW_TAP_TOKEN is set in this shell or the
///   configured ANODIZER_GITHUB_TOKEN fallback`.
/// - SSH (AUR): pass `None` and the hint points at the `publish.aur.private_key`
///   resolution + `GIT_SSH_COMMAND` env var (AUR has no env-var token fallback).
pub(crate) fn rollback_failure_warning_msg(
    publisher: &str,
    target_name: &str,
    target_url: &str,
    err: &dyn std::fmt::Display,
    env_var_hint: Option<&str>,
) -> String {
    let hint = match env_var_hint {
        Some(name) => format!(
            "; check ${} is set in this shell or the configured ANODIZER_GITHUB_TOKEN fallback",
            name
        ),
        None => String::from(
            "; check publish.aur.private_key resolves to a usable key and \
             $GIT_SSH_COMMAND points at it (AUR has no env-var token fallback)",
        ),
    };
    format!(
        "{publisher}: revert+push failed for {target_name} ({target_url}): {err}; \
         manual cleanup required at {target_url}{hint}"
    )
}

/// Emit the boilerplate for a top-level publisher struct: unit struct,
/// `new()` / `Default`, and an **inherent impl** carrying associated
/// constants for `name`, `group`, `required`, and `rollback_scope_needed`.
///
/// Why constants on an inherent impl, not a `Publisher` impl? Rust requires
/// a trait impl to live in a single `impl Trait for Type { ... }` block,
/// so we cannot split `name`/`group`/`required`/`rollback_scope_needed`
/// (constants) away from `run`/`rollback`/`preflight` (per-publisher
/// bodies). Instead, the macro pins the constants on the struct itself
/// and each publisher's `impl Publisher for $struct { ... }` block just
/// forwards to them. Per-publisher code shrinks to: the constants
/// declaration plus the three bodies that actually vary.
///
/// Usage:
/// ```ignore
/// simple_publisher!(MyPublisher, "my", Group::Assets, false, Some("scope"));
/// impl anodizer_core::Publisher for MyPublisher {
///     fn name(&self) -> &str { Self::PUBLISHER_NAME }
///     fn group(&self) -> anodizer_core::PublisherGroup { Self::PUBLISHER_GROUP }
///     fn required(&self) -> bool { Self::PUBLISHER_REQUIRED }
///     fn rollback_scope_needed(&self) -> Option<&'static str> { Self::ROLLBACK_SCOPE }
///     fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> { ... }
///     fn rollback(&self, ctx: &mut Context, ev: &PublishEvidence) -> anyhow::Result<()> { ... }
///     fn preflight(&self, ctx: &Context) -> anyhow::Result<PreflightCheck> { ... }
/// }
/// ```
macro_rules! simple_publisher {
    (
        $struct_name:ident,
        $name_str:expr,
        $group_expr:expr,
        $required:expr,
        $rollback_scope:expr $(,)?
    ) => {
        pub struct $struct_name;

        impl $struct_name {
            pub fn new() -> Self {
                Self
            }

            /// Stable lowercase publisher identifier (see [`anodizer_core::Publisher::name`]).
            pub const PUBLISHER_NAME: &'static str = $name_str;
            /// Scheduling group (see [`anodizer_core::Publisher::group`]).
            pub const PUBLISHER_GROUP: anodizer_core::PublisherGroup = $group_expr;
            /// Whether failure here fails the release (see [`anodizer_core::Publisher::required`]).
            pub const PUBLISHER_REQUIRED: bool = $required;
            /// OAuth / token scope rollback would need (see [`anodizer_core::Publisher::rollback_scope_needed`]).
            pub const ROLLBACK_SCOPE: Option<&'static str> = $rollback_scope;
        }

        impl Default for $struct_name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_empty_warning_msg_contains_publisher_and_target() {
        let msg = rollback_empty_warning_msg("artifactory", "upload URLs");
        assert!(msg.contains("artifactory"));
        assert!(msg.contains("upload URLs"));
        assert!(msg.contains("verify"));
        assert!(msg.contains("manually"));
    }

    #[test]
    fn rollback_failure_warning_msg_https_names_env_var() {
        let msg = rollback_failure_warning_msg(
            "homebrew",
            "demo",
            "https://github.com/acme/homebrew-tap.git",
            &"auth denied",
            Some("HOMEBREW_TAP_TOKEN"),
        );
        assert!(
            msg.starts_with("homebrew: revert+push failed for demo"),
            "{msg}"
        );
        assert!(
            msg.contains("https://github.com/acme/homebrew-tap.git"),
            "{msg}"
        );
        assert!(msg.contains("auth denied"), "{msg}");
        assert!(msg.contains("$HOMEBREW_TAP_TOKEN"), "{msg}");
        assert!(msg.contains("ANODIZER_GITHUB_TOKEN"), "{msg}");
        assert!(msg.contains("manual cleanup"), "{msg}");
    }

    #[test]
    fn rollback_failure_warning_msg_ssh_points_at_private_key() {
        let msg = rollback_failure_warning_msg(
            "aur",
            "demo-bin",
            "ssh://aur@aur.archlinux.org/demo-bin.git",
            &"clone failed",
            None,
        );
        assert!(
            msg.contains("aur: revert+push failed for demo-bin"),
            "{msg}"
        );
        assert!(msg.contains("publish.aur.private_key"), "{msg}");
        assert!(msg.contains("GIT_SSH_COMMAND"), "{msg}");
        assert!(!msg.contains("ANODIZER_GITHUB_TOKEN"), "{msg}");
    }

    #[test]
    fn is_top_level_block_configured_handles_none_some_empty_and_nonempty() {
        let none: Option<&Vec<u32>> = None;
        assert!(!is_top_level_block_configured(none));
        let empty: Vec<u32> = Vec::new();
        assert!(!is_top_level_block_configured(Some(&empty)));
        let one = vec![1u32];
        assert!(is_top_level_block_configured(Some(&one)));
    }
}
