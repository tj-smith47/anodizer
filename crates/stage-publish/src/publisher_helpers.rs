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

/// Canonical per-crate dispatch predicate: true when the named crate (in
/// the FULL crate universe — top-level plus workspace crates) carries the
/// publisher's `publish.<X>` block.
///
/// `block` is the publisher's single config accessor (e.g.
/// `crate::scoop::block`), shared with the registry's any-crate gate
/// ([`is_any_crate_block_configured`]) so a publisher's dispatch universe
/// and its registration gate key on the same field by construction.
pub(crate) fn is_per_crate_block_configured<T>(
    ctx: &anodizer_core::context::Context,
    crate_name: &str,
    block: impl Fn(&anodizer_core::config::PublishConfig) -> Option<&T>,
) -> bool {
    ctx.config
        .crate_universe()
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().and_then(&block).is_some())
}

/// Canonical any-crate registration gate: true when ANY crate in the full
/// crate universe carries the publisher's `publish.<X>` block. Shares the
/// same per-publisher `block` accessor as
/// [`is_per_crate_block_configured`], so a publisher's registration gate
/// cannot exclude a crate its per-crate dispatch would include.
pub(crate) fn is_any_crate_block_configured<T>(
    ctx: &anodizer_core::context::Context,
    block: impl Fn(&anodizer_core::config::PublishConfig) -> Option<&T>,
) -> bool {
    ctx.config
        .crate_universe()
        .into_iter()
        .any(|c| c.publish.as_ref().and_then(&block).is_some())
}

/// Operator-facing publisher-entry line for a per-crate publisher: names
/// the publisher and how many selected crates it is scanning. One format
/// string for every per-crate publisher, so the `starting … publish —
/// scanning` family stays a single grep surface (per-file copies kept the
/// wording aligned only by copy discipline, and upstream-AUR shipped with
/// no entry line at all).
pub(crate) fn run_start_message(publisher: &str, selected_total: usize) -> String {
    let article = if publisher.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!(
        "starting {publisher} publish — scanning {selected_total} selected crate(s) \
         for {article} {publisher} config block"
    )
}

/// Operator-facing line for a selected crate that carries no
/// `publish.<X>` block. Replaces what used to be a silent `continue` —
/// operators need to see why a per-crate publish was a no-op rather than
/// guess from a blank log. Shared wording for the same reason as
/// [`run_start_message`].
pub(crate) fn no_config_block_message(publisher: &str, crate_name: &str) -> String {
    format!("skipped {publisher} for crate '{crate_name}' — no {publisher} config block")
}

/// Resolve the effective list of crates a per-crate publisher should
/// iterate over.
///
/// - When `ctx.options.selected_crates` is non-empty: returns those names
///   verbatim (operator passed `--crate` and the run honors that scope).
/// - When `ctx.options.selected_crates` is empty: returns every crate in
///   the full crate universe (top-level + workspace crates) for which
///   `is_per_crate_configured` returns true. Uses
///   [`anodizer_core::config::Config::crate_universe`] so workspace-only
///   crates carrying a publisher block are not silently skipped — they are
///   visible under `--all` and must be equally visible in this
///   implicit-all path.
pub(crate) fn effective_publish_crates(
    ctx: &anodizer_core::context::Context,
    is_per_crate_configured: impl Fn(&anodizer_core::context::Context, &str) -> bool,
) -> Vec<String> {
    if !ctx.options.selected_crates.is_empty() {
        return ctx.options.selected_crates.clone();
    }
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| is_per_crate_configured(ctx, &c.name))
        .map(|c| c.name.clone())
        .collect()
}

/// Run a per-crate publisher body with the crate's OWN version/name/tag
/// template vars in scope, restoring the prior scope afterward.
///
/// Every per-crate publisher (`winget`, `scoop`, `krew`, `homebrew`, `nix`,
/// `aur`, `aur_source`, `chocolatey`) renders the crate's version into the
/// manifest it pushes. `Context::populate_git_vars` derives the global
/// `Version`/`Tag`/`ProjectName` from the FIRST crate's tag, so in workspace
/// per-crate INDEPENDENT-version mode every sibling would inherit the first
/// crate's version — an irreversible broken publish (each crate's manifest
/// carrying the wrong version). Wrapping the publish call in
/// [`anodizer_core::crate_scope::with_crate_scope`] re-scopes the vars to
/// THIS crate's own tag for the duration of `body`, so each manifest renders
/// under its own version. In single-crate / workspace-lockstep mode the
/// per-crate tag resolves to the same version the global context already
/// carries, so behavior is identical.
///
/// Fails loud when `crate_name` is absent from the crate universe, or when
/// the crate has no resolvable tag matching its `tag_template`: a per-crate
/// emission stamped with the wrong (first-crate) version ships a broken
/// artifact, so the error must surface locally rather than be papered over.
///
/// `resolve_tag` is the per-crate tag source — production passes
/// [`anodizer_core::crate_scope::resolve_crate_tag`]; tests inject a
/// fixed-tag closure so the version dimension can be exercised without a git
/// fixture.
pub(crate) fn with_published_crate_scope<T>(
    ctx: &mut anodizer_core::context::Context,
    crate_name: &str,
    resolve_tag: &dyn Fn(
        &anodizer_core::context::Context,
        &anodizer_core::config::CrateConfig,
    ) -> Option<String>,
    body: impl FnOnce(&mut anodizer_core::context::Context) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    // Cloned (not borrowed) because `body` takes `ctx` mutably while the
    // scope guard still needs the crate's tag template.
    let crate_cfg = ctx
        .config
        .crate_universe()
        .into_iter()
        .find(|c| c.name == crate_name)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "publish: crate '{crate_name}' selected for a per-crate emission \
                 is not present in the crate universe"
            )
        })?;
    anodizer_core::crate_scope::with_crate_scope(ctx, &crate_cfg, resolve_tag, body)
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
///   configured ANODIZER_GITHUB_TOKEN or GITHUB_TOKEN fallback` (the
///   fallback ladder is interpolated from
///   [`anodizer_core::git::github_token_env_hint`], never hand-spelled).
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
            "; check ${} is set in this shell or the configured {} fallback",
            name,
            anodizer_core::git::github_token_env_hint()
        ),
        None => String::from(
            "; check publish.aur.private_key resolves to a usable key and \
             $GIT_SSH_COMMAND points at it (AUR has no env-var token fallback)",
        ),
    };
    format!(
        "{publisher} revert+push failed for {target_name} ({target_url}): {err}; \
         manual cleanup required at {target_url}{hint}"
    )
}

/// Emit the boilerplate for a top-level publisher struct: a struct with
/// `Option<bool>` override fields, `new()` / `Default`, `with_overrides()`,
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
/// `with_overrides(required, retain)` passes config-level `Option<bool>`
/// values into the struct. `None` falls through to the built-in defaults;
/// `Some(v)` overrides. `resolved_required(&self)` and
/// `resolved_retain_on_rollback(&self)` perform the resolution so the override
/// logic lives in exactly one place.
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
            /// Config-level override for `retain_on_rollback()`. `None` means
            /// the publisher participates in rollback (the default); `Some(true)`
            /// opts this publisher out of rollback so its successful work is left
            /// in place even when the pipeline rolls back.
            retain_on_rollback_override: Option<bool>,
        }

        impl $struct_name {
            pub fn new() -> Self {
                Self {
                    required_override: None,
                    retain_on_rollback_override: None,
                }
            }

            /// Construct with both config-supplied overrides.
            ///
            /// Convenience constructor for dispatch sites that read both
            /// `required` and `retain_on_rollback` from the publisher's config
            /// struct in a single call.
            pub fn with_overrides(
                required_override: Option<bool>,
                retain_on_rollback_override: Option<bool>,
            ) -> Self {
                Self {
                    required_override,
                    retain_on_rollback_override,
                }
            }

            /// Combine the config-supplied override with `PUBLISHER_REQUIRED`.
            ///
            /// The `Publisher::required()` trait impl forwards here so the
            /// override-resolution logic lives in exactly one place and a
            /// publisher cannot silently lose the override.
            pub fn resolved_required(&self) -> bool {
                self.required_override.unwrap_or(Self::PUBLISHER_REQUIRED)
            }

            /// Resolve `retain_on_rollback` from the config-supplied override.
            ///
            /// Returns `true` when the config sets `retain_on_rollback: true`,
            /// `false` otherwise (the default — rollback runs normally).
            pub fn resolved_retain_on_rollback(&self) -> bool {
                self.retain_on_rollback_override.unwrap_or(false)
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

// ---------------------------------------------------------------------------
// Preflight requirement helpers shared by the per-publisher
// `Publisher::requirements` implementations
// ---------------------------------------------------------------------------

/// Derive the env requirement for a git/GitHub-repo-backed publisher's
/// token, mirroring `util::resolve_repo_token`'s ladder: explicit
/// `--token` option → `repo.token` (templated) → preferred env var →
/// `ANODIZER_GITHUB_TOKEN` → `GITHUB_TOKEN`.
///
/// Returns `None` when no env var is needed (explicit `--token`, or a
/// literal non-templated `repo.token` in config).
pub(crate) fn repo_token_requirement(
    ctx: &anodizer_core::context::Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    preferred_env: Option<&str>,
) -> Option<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::template_env_refs;
    if ctx.options.token.as_deref().is_some_and(|t| !t.is_empty()) {
        return None;
    }
    if let Some(r) = repo
        && let Some(tok) = r.token.as_deref()
        && !tok.is_empty()
    {
        let refs = template_env_refs(tok);
        if refs.is_empty() {
            // Literal token value in config — present by definition.
            return None;
        }
        return Some(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
    }
    let mut vars: Vec<String> = Vec::new();
    if let Some(p) = preferred_env {
        vars.push(p.to_string());
    }
    vars.extend(
        anodizer_core::git::GITHUB_TOKEN_ENV_LADDER
            .iter()
            .map(|v| v.to_string()),
    );
    Some(anodizer_core::EnvRequirement::EnvAnyOf { vars })
}

/// Full requirement set for a git-repo-backed publisher: the `git` tool
/// plus either the token ladder (HTTPS pushes) or the SSH key material
/// referenced by `repo.git` (SSH pushes).
pub(crate) fn git_repo_requirements(
    ctx: &anodizer_core::context::Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    preferred_env: Option<&str>,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::template_env_refs;
    let mut out = vec![anodizer_core::EnvRequirement::Tool {
        name: "git".to_string(),
    }];
    let git_cfg = repo.and_then(|r| r.git.as_ref());
    let ssh_url = git_cfg
        .and_then(|g| g.url.as_deref())
        .is_some_and(|u| !u.is_empty());
    if ssh_url {
        for field in [
            git_cfg.and_then(|g| g.private_key.as_deref()),
            git_cfg.and_then(|g| g.ssh_command.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            let refs = template_env_refs(field);
            if !refs.is_empty() {
                out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
            }
        }
    } else if let Some(req) = repo_token_requirement(ctx, repo, preferred_env) {
        out.push(req);
    }
    out
}

/// Requirement set for an AUR-style ssh-push publisher: the `git` tool
/// plus the ssh key material referenced by `private_key` /
/// `git_ssh_command` (both templated; `{{ .Env.AUR_SSH_KEY }}` is the
/// canonical shape). A `private_key` that is exactly one env reference is
/// declared as validatable key material; composite templates degrade to a
/// presence check on the referenced vars. A configured key without a
/// custom `git_ssh_command` also demands the `ssh` binary: the clone path
/// writes the key to disk and sets `GIT_SSH_COMMAND` to an `ssh -i …`
/// invocation, which git spawns — a custom command replaces that
/// invocation wholesale, so it lifts the demand.
pub(crate) fn aur_ssh_requirements(
    private_key: Option<&str>,
    git_ssh_command: Option<&str>,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::{sole_env_ref, template_env_refs};
    let mut out = vec![anodizer_core::EnvRequirement::Tool {
        name: "git".to_string(),
    }];
    if let Some(pk) = private_key.filter(|v| !v.is_empty()) {
        if git_ssh_command.filter(|v| !v.is_empty()).is_none() {
            out.push(anodizer_core::EnvRequirement::Tool {
                name: "ssh".to_string(),
            });
        }
        if let Some(var) = sole_env_ref(pk) {
            out.push(anodizer_core::EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::SshPrivate,
                var,
            });
        } else {
            let refs = template_env_refs(pk);
            if !refs.is_empty() {
                out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
            }
        }
    } else if let Some(cmd) = git_ssh_command.filter(|v| !v.is_empty()) {
        let refs = template_env_refs(cmd);
        if !refs.is_empty() {
            out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
        }
    }
    out
}

/// Requirement for a secret resolved as "templated config value, else env
/// var `fallback_env`": env refs of the config value when templated, the
/// fallback var when the config value is absent, nothing when the config
/// holds a literal.
pub(crate) fn secret_requirement(
    config_value: Option<&str>,
    fallback_env: &str,
) -> Option<anodizer_core::EnvRequirement> {
    anodizer_core::env_preflight::secret_requirement(config_value, fallback_env)
}

/// True when a publisher entry is statically inactive for this run: its
/// `skip:` / `skip_upload:` evaluates truthy, or its `if:` condition
/// renders falsy. Mirrors the run-path gating for requirement derivation —
/// a `skip: true` entry must not demand credentials from preflight.
/// Anything unrenderable is treated as ACTIVE so preflight over-collects
/// rather than silently under-collecting.
pub(crate) fn entry_inactive(
    ctx: &anodizer_core::context::Context,
    skip: Option<&anodizer_core::config::StringOrBool>,
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    if_condition: Option<&str>,
) -> bool {
    anodizer_core::env_preflight::entry_inactive(ctx, skip, skip_upload, if_condition)
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
            msg.starts_with("homebrew revert+push failed for demo"),
            "{msg}"
        );
        assert!(
            msg.contains("https://github.com/acme/homebrew-tap.git"),
            "{msg}"
        );
        assert!(msg.contains("auth denied"), "{msg}");
        assert!(msg.contains("$HOMEBREW_TAP_TOKEN"), "{msg}");
        // The fallback ladder names EVERY var the resolution chain reads,
        // in precedence order (rendered from GITHUB_TOKEN_ENV_LADDER).
        assert!(
            msg.contains("ANODIZER_GITHUB_TOKEN or GITHUB_TOKEN"),
            "{msg}"
        );
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
        assert!(msg.contains("aur revert+push failed for demo-bin"), "{msg}");
        assert!(msg.contains("publish.aur.private_key"), "{msg}");
        assert!(msg.contains("GIT_SSH_COMMAND"), "{msg}");
        assert!(!msg.contains("ANODIZER_GITHUB_TOKEN"), "{msg}");
    }

    /// A configured private key with no custom `git_ssh_command` rides the
    /// default `ssh -i …` GIT_SSH_COMMAND, so the `ssh` binary itself must
    /// be demanded alongside `git` and the key material.
    #[test]
    fn aur_ssh_requirements_default_key_demands_ssh_tool() {
        let reqs = aur_ssh_requirements(Some("{{ .Env.AUR_SSH_KEY }}"), None);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "ssh"
            )),
            "private key without git_ssh_command must demand ssh: {reqs:?}"
        );
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::KeyEnv { var, .. } if var == "AUR_SSH_KEY"
            )),
            "private key env ref must still be demanded as key material: {reqs:?}"
        );
    }

    /// A custom `git_ssh_command` replaces the default ssh invocation
    /// wholesale (git spawns the configured command instead), so `ssh`
    /// must not be demanded even when a private key is also set.
    #[test]
    fn aur_ssh_requirements_custom_ssh_command_lifts_ssh_tool() {
        let ssh_demanded = |reqs: &[anodizer_core::EnvRequirement]| {
            reqs.iter().any(|r| {
                matches!(
                    r,
                    anodizer_core::EnvRequirement::Tool { name } if name == "ssh"
                )
            })
        };
        let reqs = aur_ssh_requirements(
            Some("{{ .Env.AUR_SSH_KEY }}"),
            Some("ssh-wrapper -o IdentityAgent=none"),
        );
        assert!(
            !ssh_demanded(&reqs),
            "custom git_ssh_command must lift the ssh demand: {reqs:?}"
        );
        let reqs = aur_ssh_requirements(None, Some("{{ .Env.AUR_SSH_CMD }}"));
        assert!(
            !ssh_demanded(&reqs),
            "git_ssh_command without a key must not demand ssh: {reqs:?}"
        );
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

    /// One pin for the cross-publisher operator-line wording. Every
    /// per-crate publisher (krew, nix, homebrew, scoop, aur, chocolatey,
    /// winget, aur_source) formats its entry + no-config-block lines
    /// through these helpers, so this single test replaces the per-file
    /// copies that kept the `starting … publish — scanning` grep surface
    /// aligned only by copy discipline.
    #[test]
    fn shared_operator_line_wording_is_stable() {
        assert_eq!(
            run_start_message("scoop", 3),
            "starting scoop publish — scanning 3 selected crate(s) for a scoop config block"
        );
        // Vowel-initial publishers take "an" ("an aur", "an aur_source"),
        // matching aur's original hand-written entry line.
        assert_eq!(
            run_start_message("aur", 2),
            "starting aur publish — scanning 2 selected crate(s) for an aur config block"
        );
        assert_eq!(
            no_config_block_message("krew", "demo"),
            "skipped krew for crate 'demo' — no krew config block"
        );
    }
}
