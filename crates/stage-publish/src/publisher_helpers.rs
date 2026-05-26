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
//! portably). Publishers whose `rollback()` is a no-op when evidence is
//! empty call this helper for the user-facing warn line.
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

/// Resolve the effective list of crates a per-crate publisher should
/// iterate over.
///
/// - When `ctx.options.selected_crates` is non-empty: returns those names
///   verbatim (operator passed `--crate` and the run honors that scope).
/// - When `ctx.options.selected_crates` is empty: returns every crate in
///   the full crate universe (top-level + workspace crates) for which
///   `is_per_crate_configured` returns true. Uses `util::all_crates` so
///   workspace-only crates carrying a publisher block are not silently
///   skipped — they are visible under `--all` and must be equally visible
///   in this implicit-all path.
pub(crate) fn effective_publish_crates(
    ctx: &anodizer_core::context::Context,
    is_per_crate_configured: impl Fn(&anodizer_core::context::Context, &str) -> bool,
) -> Vec<String> {
    if !ctx.options.selected_crates.is_empty() {
        return ctx.options.selected_crates.clone();
    }
    crate::util::all_crates(ctx)
        .into_iter()
        .filter(|c| is_per_crate_configured(ctx, &c.name))
        .map(|c| c.name)
        .collect()
}

/// Canonical wording for a PR-based publisher's rollback failure warn line.
///
/// PR-based rollbacks shell out to `git revert HEAD --no-edit` + `git push`.
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

/// Emit the boilerplate for a top-level publisher struct: a struct with an
/// `Option<bool>` override field, `new()` / `Default`, `with_required()`,
/// and an **inherent impl** carrying associated constants for `name`, `group`,
/// `required`, and `rollback_scope_needed` plus a `resolved_required()`
/// helper that combines the config override with `PUBLISHER_REQUIRED`.
///
/// Why constants on an inherent impl, not a `Publisher` impl? Rust requires
/// a trait impl to live in a single `impl Trait for Type { ... }` block,
/// so we cannot split `name`/`group`/`required`/`rollback_scope_needed`
/// (constants) away from `run`/`rollback`/`preflight` (per-publisher
/// bodies). Instead, the macro pins the constants on the struct itself
/// and each publisher's `impl Publisher for $struct { ... }` block just
/// forwards to them.
///
/// `with_required(override)` passes a config-level `Option<bool>` into the
/// struct. `None` falls through to `PUBLISHER_REQUIRED`; `Some(v)` overrides
/// it. `resolved_required(&self)` performs the resolution so per-publisher
/// trait impls forward via a single `Self::resolved_required(self)` call —
/// the override logic lives in exactly one place and a future publisher
/// author cannot silently drop the override by forgetting the `unwrap_or`
/// expression.
///
/// Usage:
/// ```ignore
/// simple_publisher!(MyPublisher, "my", Group::Assets, false, Some("scope"));
/// impl anodizer_core::Publisher for MyPublisher {
///     fn name(&self) -> &str { Self::PUBLISHER_NAME }
///     fn group(&self) -> anodizer_core::PublisherGroup { Self::PUBLISHER_GROUP }
///     fn required(&self) -> bool { Self::resolved_required(self) }
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
        pub struct $struct_name {
            /// Config-level override for `required()`. `None` falls through to
            /// `PUBLISHER_REQUIRED`; `Some(v)` overrides it.
            required_override: Option<bool>,
        }

        impl $struct_name {
            pub fn new() -> Self {
                Self {
                    required_override: None,
                }
            }

            /// Construct with a config-supplied `required` override.
            ///
            /// Pass the `Option<bool>` read from the publisher's config struct
            /// (e.g. `ctx.config.crates[].publish.homebrew.required`). `None`
            /// keeps the built-in default; `Some(v)` overrides it for this run.
            pub fn with_required(required_override: Option<bool>) -> Self {
                Self { required_override }
            }

            /// Combine the config-supplied override with `PUBLISHER_REQUIRED`.
            ///
            /// The `Publisher::required()` trait impl forwards here so the
            /// override-resolution logic lives in exactly one place and a
            /// publisher cannot silently lose the override.
            pub fn resolved_required(&self) -> bool {
                self.required_override.unwrap_or(Self::PUBLISHER_REQUIRED)
            }

            /// Stable lowercase publisher identifier (see [`anodizer_core::Publisher::name`]).
            pub const PUBLISHER_NAME: &'static str = $name_str;
            /// Scheduling group (see [`anodizer_core::Publisher::group`]).
            pub const PUBLISHER_GROUP: anodizer_core::PublisherGroup = $group_expr;
            /// Built-in default for whether failure here fails the release (see [`anodizer_core::Publisher::required`]).
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
    fn effective_publish_crates_returns_selected_verbatim_when_non_empty() {
        use anodizer_core::config::{CrateConfig, PublishConfig};
        use anodizer_core::test_helpers::TestContextBuilder;
        let ctx = TestContextBuilder::new()
            .crates(vec![
                CrateConfig {
                    name: "alpha".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
                CrateConfig {
                    name: "beta".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .selected_crates(vec!["beta".to_string()])
            .build();
        // The predicate returns true for both — but with a non-empty
        // selection, the helper MUST honor it verbatim and skip the
        // implicit-all branch entirely.
        let names = effective_publish_crates(&ctx, |_, _| true);
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn effective_publish_crates_implicit_all_walks_configured_crates() {
        use anodizer_core::config::{CrateConfig, PublishConfig};
        use anodizer_core::test_helpers::TestContextBuilder;
        let ctx = TestContextBuilder::new()
            .crates(vec![
                CrateConfig {
                    name: "alpha".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
                CrateConfig {
                    name: "beta".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        // Predicate matches `alpha` and `gamma` only — `beta` is filtered
        // out so the implicit-all branch must skip it. Order matches the
        // config crate order.
        let names = effective_publish_crates(&ctx, |_, name| name == "alpha" || name == "gamma");
        assert_eq!(names, vec!["alpha".to_string(), "gamma".to_string()]);
    }

    #[test]
    fn effective_publish_crates_implicit_all_returns_empty_when_no_configured_crate() {
        use anodizer_core::test_helpers::TestContextBuilder;
        let ctx = TestContextBuilder::new().build();
        let names = effective_publish_crates(&ctx, |_, _| true);
        assert!(
            names.is_empty(),
            "no crates configured → empty vec, got {names:?}"
        );
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

    /// `effective_publish_crates` must surface workspace-only crates under
    /// implicit-all. A crate living exclusively in `workspaces[].crates` (not
    /// in `config.crates`) that carries a publisher block must be returned
    /// when no explicit `--crate` selection is present — matching the behavior
    /// of `--all` in the CLI dispatcher.
    #[test]
    fn effective_publish_crates_implicit_all_includes_workspace_only_crates() {
        use anodizer_core::config::{CrateConfig, PublishConfig, WorkspaceConfig};
        use anodizer_core::test_helpers::TestContextBuilder;

        // Top-level crates list is empty; the crate lives only in a workspace.
        let ws_crate = CrateConfig {
            name: "ws-only".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![])
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![ws_crate],
                ..Default::default()
            }])
            .build();

        // Predicate always returns true (the publisher is configured for any name).
        let names = effective_publish_crates(&ctx, |_, _| true);
        assert_eq!(
            names,
            vec!["ws-only".to_string()],
            "workspace-only crate must appear under implicit-all"
        );
    }
}
