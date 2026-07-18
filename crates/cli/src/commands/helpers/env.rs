use anodizer_core::config::{Config, ForceTokenKind};
use anodizer_core::context::Context;
use anyhow::Context as _;

/// Set a process-level environment variable.
///
/// # Safety contract
///
/// `std::env::set_var` is unsafe because it mutates global process state that
/// other threads may be reading concurrently.  This function must ONLY be
/// called during single-threaded pipeline setup (i.e., inside `setup_env`)
/// before any worker threads are spawned.  All later stages that need env
/// values should read from the `Context` template vars or pass them
/// explicitly via `Command::envs()`.
pub(super) fn set_env_var_single_threaded(key: &str, value: &str) {
    // SAFETY: Caller guarantees no other threads exist yet.
    unsafe { std::env::set_var(key, value) };
}

/// Resolve the effective `force_token` kind — config field first, then the
/// `ANODIZER_FORCE_TOKEN` env var, then the `GORELEASER_FORCE_TOKEN` compat
/// fallback. Returns `None` if nothing is set (or the value isn't a recognised
/// backend).
///
/// Extracted so `setup_env` and `resolve_scm_token_type` can't drift — adding
/// a new backend only needs to be wired in this one place. Reads env vars
/// through the injected `EnvSource` so tests stay off process-env mutation.
pub(super) fn resolve_force_token_with_env<E: anodizer_core::env_source::EnvSource + ?Sized>(
    config: &Config,
    env: &E,
) -> Option<ForceTokenKind> {
    config.force_token.as_ref().cloned().or_else(|| {
        let env_val = env
            .var("ANODIZER_FORCE_TOKEN")
            .or_else(|| env.var("GORELEASER_FORCE_TOKEN"))?;
        match env_val.to_lowercase().as_str() {
            "github" => Some(ForceTokenKind::GitHub),
            "gitlab" => Some(ForceTokenKind::GitLab),
            "gitea" => Some(ForceTokenKind::Gitea),
            _ => None,
        }
    })
}

/// Process-env convenience wrapper over [`resolve_force_token_with_env`].
fn resolve_force_token(config: &Config) -> Option<ForceTokenKind> {
    resolve_force_token_with_env(config, &anodizer_core::env_source::ProcessEnvSource)
}

/// Combine `defaults.env` and top-level `config.env` into a single list with
/// deterministic precedence: defaults entries come first so any same-keyed
/// entry in `config.env` clobbers the defaults version on the
/// last-one-wins-per-key application path inside `setup_env`.
///
/// Returns `None` when both inputs are `None`. Returns the cloned non-None
/// input when only one side is set.
pub(super) fn merge_env_with_defaults(
    defaults_env: Option<&Vec<String>>,
    config_env: Option<&Vec<String>>,
) -> Option<Vec<String>> {
    match (defaults_env, config_env) {
        (None, None) => None,
        (Some(d), None) => Some(d.clone()),
        (None, Some(c)) => Some(c.clone()),
        (Some(d), Some(c)) => {
            let mut v = Vec::with_capacity(d.len() + c.len());
            v.extend(d.iter().cloned());
            v.extend(c.iter().cloned());
            Some(v)
        }
    }
}

/// Extract the variable keys a `variables:` template value references via the
/// `.Var.<key>` / `Var.<key>` namespace.
///
/// Used only to surface forward-reference warnings — it is a deliberately
/// shallow scan (it does not parse Tera), matching `<name>` against the
/// `[A-Za-z0-9_]` key charset. Over-matching is harmless: the caller only
/// warns when the name is also a declared sibling key, so non-variable
/// matches (`.Var` followed by something that isn't a configured key) are
/// silently ignored.
pub(super) fn referenced_var_keys(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = template.as_bytes();
    // Walk each `Var.` occurrence and lift the following identifier.
    for (idx, _) in template.match_indices("Var.") {
        let start = idx + "Var.".len();
        let key_len = bytes[start..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
            .count();
        if key_len > 0 {
            out.push(&template[start..start + key_len]);
        }
    }
    out
}

/// Load process environment variables, `.env` files, and user-defined env vars
/// into the context's template variables.
///
/// Loading order (later wins):
/// 1. All process environment variables (`std::env::vars()`)
/// 2. Variables from `.env` files specified in config
/// 3. Explicit `env:` map entries — `defaults.env` first, then `config.env`
///    (so per-config entries override defaults on duplicate keys)
///
/// This ensures config-defined env vars always take precedence over process
/// environment, where all process env vars are
/// accessible in templates via `{{ .Env.VAR }}`.
pub fn setup_env(
    ctx: &mut Context,
    config: &Config,
    log: &anodizer_core::log::StageLogger,
) -> anyhow::Result<()> {
    // Load ALL process environment variables first (lowest priority)
    for (key, value) in ctx.env_source().vars() {
        ctx.template_vars_mut().set_env(&key, &value);
    }

    // Load env files into template context (overrides process env).
    // Supports both list form (array of .env files) and struct form (token file paths).
    // These are user-configured, so use set_config_env (safe for cross-platform
    // serialization and subprocess injection).
    if let Some(ref env_files_config) = config.env_files {
        match env_files_config {
            anodizer_core::config::EnvFilesConfig::List(files) => {
                let env_vars = anodizer_core::config::load_env_files(files, log, ctx.is_strict())
                    .map_err(anyhow::Error::msg)?;
                for (key, value) in &env_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                }
            }
            anodizer_core::config::EnvFilesConfig::TokenFiles(token_config) => {
                let token_vars = anodizer_core::config::load_token_files(token_config, log)
                    .map_err(anyhow::Error::msg)?;
                for (key, value) in &token_vars {
                    ctx.template_vars_mut().set_config_env(key, value);
                    set_env_var_single_threaded(key, value);
                }
            }
        }
    } else {
        // always check default
        // token file paths even when env_files is not configured.
        let default_config = anodizer_core::config::EnvFilesTokenConfig::default();
        let token_vars = anodizer_core::config::load_token_files(&default_config, log)
            .map_err(anyhow::Error::msg)?;
        for (key, value) in &token_vars {
            ctx.template_vars_mut().set_config_env(key, value);
            set_env_var_single_threaded(key, value);
        }
    }

    // Populate user-defined env vars into template context (highest priority).
    // Env values are rendered through the template engine.
    let merged_env = merge_env_with_defaults(
        config.defaults.as_ref().and_then(|d| d.env.as_ref()),
        config.env.as_ref(),
    );
    if let Some(ref env_list) = merged_env {
        let rendered_pairs =
            anodizer_core::config::render_env_entries(env_list, |v| ctx.render_template(v))
                .with_context(|| "config.env: parse and render entries")?;
        for (key, rendered) in rendered_pairs {
            ctx.template_vars_mut().set_config_env(&key, &rendered);
            // Also set in the process environment so that child processes which
            // inherit env (docker, lipo, rustup, git, hook scripts) see these
            // values. Some commands use explicit `.envs()`, but many rely on
            // process-level inheritance.
            //
            // SAFETY: This is called during single-threaded pipeline setup in
            // `setup_env`, before any worker threads are spawned. No concurrent
            // readers of the process environment exist at this point.
            set_env_var_single_threaded(&key, &rendered);
        }
    }

    // Populate user-defined custom variables into template context.
    //
    // Iteration is sorted by key (BTreeMap) so cross-variable references
    // resolve deterministically: a value like `b: "{{ .Var.a }}_v2"`
    // sees `a` IF `a` sorts earlier than `b`. The single-pass model is
    // documented behaviour — variables that reference other variables
    // must rely on the alphabetical-key ordering, or refer through
    // `{{ .Env.* }}`.
    //
    // Render errors are hard: an unknown variable / typo in a template
    // expression (`{{ .Tagg }}`) fails the load instead of silently
    // passing the literal `{{ }}` through to a publisher (homebrew,
    // scoop, nix, …) where it would surface only after publication.
    if let Some(ref vars_map) = config.variables {
        // Keys already rendered + visible to later values. BTreeMap iteration is
        // alphabetical, so a value referencing a sibling key that sorts LATER
        // (a forward reference) renders against an unset `.Var.<name>` — which,
        // when guarded with `| default(value="")`, silently yields empty. Warn
        // so the operator isn't surprised by a blank substitution; the fix is
        // to rename the key so it sorts after its dependency.
        let mut defined: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (key, value) in vars_map {
            for referenced in referenced_var_keys(value) {
                if vars_map.contains_key(referenced) && !defined.contains(referenced) {
                    log.warn(&format!(
                        "variables.{key} references variable '{referenced}' that is \
                         defined later (variables resolve in alphabetical key order, so \
                         '{referenced}' is still unset here and renders empty). Rename so \
                         '{key}' sorts after '{referenced}'."
                    ));
                }
            }
            let rendered = ctx
                .render_template(value)
                .with_context(|| format!("variables.{key}: failed to render template '{value}'"))?;
            ctx.template_vars_mut().set_custom_var(key, &rendered);
            defined.insert(key.as_str());
        }
    }

    // When force_token is active, clear non-forced
    // token env vars BEFORE the multi-token check so it cannot fire.
    let resolved_force = resolve_force_token(config);
    if let Some(ref forced) = resolved_force {
        // Remove env vars for non-forced token types so downstream code
        // only sees the forced provider's token.
        let keep_github = matches!(forced, ForceTokenKind::GitHub);
        let keep_gitlab = matches!(forced, ForceTokenKind::GitLab);
        let keep_gitea = matches!(forced, ForceTokenKind::Gitea);
        if !keep_github {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe {
                std::env::remove_var("GITHUB_TOKEN");
                std::env::remove_var("ANODIZER_GITHUB_TOKEN");
            }
        }
        if !keep_gitlab {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe { std::env::remove_var("GITLAB_TOKEN") };
        }
        if !keep_gitea {
            // SAFETY: single-threaded pipeline setup, see set_env_var_single_threaded.
            unsafe { std::env::remove_var("GITEA_TOKEN") };
        }
    }

    // Multiple-token detection.
    // When multiple SCM tokens are set without force_token, error early.
    if resolved_force.is_none() {
        // Empty-filtered on every leg: a blank `TOKEN=""` (the shape GitHub
        // Actions materializes for a missing secret) is not a configured token
        // and must not trip the multi-token ambiguity guard. GitHub routes
        // through the canonical resolver; GitLab/Gitea filter inline since they
        // have no canonical resolver of their own.
        let has_github =
            anodizer_core::git::resolve_github_token_with_env(None, &|key| ctx.env_var(key))
                .is_some();
        let has_gitlab = ctx.env_var("GITLAB_TOKEN").is_some_and(|v| !v.is_empty());
        let has_gitea = ctx.env_var("GITEA_TOKEN").is_some_and(|v| !v.is_empty());
        let count = [has_github, has_gitlab, has_gitea]
            .iter()
            .filter(|&&b| b)
            .count();
        if count > 1 {
            anyhow::bail!(
                "multiple SCM tokens set simultaneously ({}). Set force_token in config \
                 or ANODIZER_FORCE_TOKEN env var to specify which to use.",
                [
                    if has_github {
                        Some("GITHUB_TOKEN")
                    } else {
                        None
                    },
                    if has_gitlab {
                        Some("GITLAB_TOKEN")
                    } else {
                        None
                    },
                    if has_gitea { Some("GITEA_TOKEN") } else { None },
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
            );
        }
    }

    // Missing token hard error.
    // Error early if no SCM token and the pipeline needs one.
    // Snapshot mode, dry-run, and release.skip can proceed without a token.
    //
    // `--publish-only` defers the token check to the config-derived
    // environment preflight (the github-release publisher's token ladder
    // plus the sign stage's `KeyEnv` requirements), which validates token
    // and sign-key material together and self-gates per resolved publisher
    // surface. If setup_env bailed here first, publish-only would never get
    // a chance to emit that richer per-publisher error or honor
    // `--no-preflight`. The dispatcher enforces the env preflight
    // downstream so dropping it here doesn't widen the hole.
    if ctx.options.token.is_none()
        && !ctx.is_snapshot()
        && !ctx.is_dry_run()
        && !ctx.options.publish_only
        && !ctx.options.preflight_secrets
    {
        let universe = config.crate_universe();
        let release_skipped = match universe
            .first()
            .and_then(|c| c.release.as_ref()?.skip.as_ref())
        {
            Some(d) => d
                .try_evaluates_to_true(|t| ctx.render_template(t))
                .with_context(|| "release: render skip template")?,
            None => false,
        };
        let needs_token = universe.iter().any(|c| c.release.is_some())
            && !ctx.should_skip("release")
            && !release_skipped;
        if needs_token {
            let hint = match ctx.token_type {
                anodizer_core::scm::ScmTokenType::GitLab => {
                    "no GitLab token found. Set GITLAB_TOKEN.".to_string()
                }
                anodizer_core::scm::ScmTokenType::Gitea => {
                    "no Gitea token found. Set GITEA_TOKEN.".to_string()
                }
                anodizer_core::scm::ScmTokenType::GitHub => {
                    // The release surface accepts `--token`, so the hint is
                    // the --token-inclusive ladder like its siblings.
                    format!(
                        "no GitHub token found: {}.",
                        anodizer_core::git::github_token_hint()
                    )
                }
            };
            anyhow::bail!("{}", hint);
        }
    }

    Ok(())
}
