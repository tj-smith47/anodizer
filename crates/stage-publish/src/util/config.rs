//! Config / context lookups & resolution helpers shared across publishers.
//!
//! - Publisher config lookup (`get_publish_config`).
//! - Artifact-kind resolution (`resolve_artifact_kind`).
//! - Token / secret resolution (`resolve_token`, `resolve_repo_token`,
//!   `resolve_secret_name`).
//! - Repository owner/name extraction (`resolve_repo_owner_name`).
//! - Skip-gate evaluation (`should_skip_upload`, `should_skip_publisher_with_if`).

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{CrateConfig, PublishConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Secret-name resolution
// ---------------------------------------------------------------------------

/// Resolve a secret/token env var name from config with template rendering.
pub(crate) fn resolve_secret_name(
    ctx: &Context,
    secret_name: Option<&str>,
    default: &str,
) -> String {
    let name = secret_name.unwrap_or(default);
    ctx.render_template(name)
        .unwrap_or_else(|_| name.to_string())
}

// ---------------------------------------------------------------------------
// Publisher config lookup
// ---------------------------------------------------------------------------

/// Look up a crate's config and its `publish` section by name, returning a
/// descriptive error when either is missing.
///
/// Resolves against the full crate universe
/// ([`anodizer_core::config::Config::crate_universe`]) — top-level
/// `ctx.config.crates` PLUS every `ctx.config.workspaces[].crates` — so a
/// workspace-only crate (the only shape in a pure-workspace config like a
/// multi-crate monorepo) is found by name. Without the workspace
/// fallthrough every per-publisher lookup (nix, homebrew, scoop, aur, krew,
/// winget, chocolatey) — and the snapshot emission validator that drives
/// them — would `bail!` "not found" for a crate defined under `workspaces:`.
pub(crate) fn get_publish_config<'a>(
    ctx: &'a Context,
    crate_name: &str,
    label: &str,
) -> Result<(&'a CrateConfig, &'a PublishConfig)> {
    let crate_cfg = find_crate_in_universe(ctx, crate_name)
        .ok_or_else(|| anyhow::anyhow!("{label}: crate '{crate_name}' not found in config"))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{label}: no publish config for '{crate_name}'"))?;

    Ok((crate_cfg, publish))
}

/// Borrow a crate by name from the full crate universe
/// ([`anodizer_core::config::Config::crate_universe`] — top-level wins on a
/// name collision). Returns a reference tied to `ctx` so callers can keep
/// zero-copy `&CrateConfig` access without cloning the whole universe.
pub(crate) fn find_crate_in_universe<'a>(
    ctx: &'a Context,
    crate_name: &str,
) -> Option<&'a CrateConfig> {
    ctx.config.find_crate(crate_name)
}

// ---------------------------------------------------------------------------
// Artifact kind resolution
// ---------------------------------------------------------------------------

/// Map the `use` config value (e.g. "archive", "msi", "nsis") to an
/// `ArtifactKind`.  Defaults to `Archive` when the value is `None` or
/// unrecognised.
pub(crate) fn resolve_artifact_kind(use_value: Option<&str>) -> ArtifactKind {
    match use_value {
        Some("msi") | Some("nsis") => ArtifactKind::Installer,
        // "archive" or anything else defaults to Archive
        _ => ArtifactKind::Archive,
    }
}

// ---------------------------------------------------------------------------
// Token resolution
// ---------------------------------------------------------------------------

/// Resolve an auth token from the context, then a publisher-specific env var,
/// then `ANODIZER_GITHUB_TOKEN`, then the generic `GITHUB_TOKEN` env var.
///
/// Env reads route through the [`Context`]'s injected env source so unit
/// tests can drive the fallback ladder via a
/// [`MapEnvSource`](anodizer_core::MapEnvSource) without mutating the
/// process env.
pub(crate) fn resolve_token(ctx: &Context, env_var: Option<&str>) -> Option<String> {
    // Filter empty strings: GitHub Actions sets env vars from non-existent
    // secrets to "", which would short-circuit the fallback chain and prevent
    // GITHUB_TOKEN from being used.
    let non_empty = |s: String| if s.is_empty() { None } else { Some(s) };
    ctx.options
        .token
        .clone()
        .and_then(non_empty)
        .or_else(|| env_var.and_then(|v| ctx.env_var(v)).and_then(non_empty))
        .or_else(|| {
            anodizer_core::git::resolve_github_token_with_env(None, &|key| ctx.env_var(key))
        })
}

// ---------------------------------------------------------------------------
// Repository owner/name resolution
// ---------------------------------------------------------------------------

/// Resolve repository owner/name from a RepositoryConfig.
///
/// Returns `Some((owner, name))` when both fields are populated, `None`
/// when neither is set or only one is set. Callers chain `.ok_or_else()`
/// to surface a missing-repository error.
pub(crate) fn resolve_repo_owner_name(
    repo: Option<&anodizer_core::config::RepositoryConfig>,
) -> Option<(String, String)> {
    repo.and_then(|r| match (r.owner.as_deref(), r.name.as_deref()) {
        (Some(o), Some(n)) => Some((o.to_string(), n.to_string())),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Skip-gate evaluation
// ---------------------------------------------------------------------------

/// Resolve `skip_upload` to a boolean for publisher entry-points, emitting
/// the canonical skip-upload log line when the upload is skipped.
///
/// Accepts the StringOrBool field directly. Templates are rendered via
/// `ctx.render_template`. Returns:
/// - `true` if the value renders to literal "true", or to "auto" while the
///   release is a prerelease (template var `Prerelease` is non-empty).
/// - `false` if `None`, or renders to "false"/"".
/// - `false` with a warn if the value is an unrecognized string.
///
/// When the decision is `true` and `label` is `Some`, this emits the single
/// canonical operator-facing line `skipped <label> upload — skip_upload is
/// set` (the same phrasing [`should_skip_publisher_with_if`] uses). The line
/// lives here so every publisher's skip-upload wording is identical; callers
/// pass `None` only on render/validation paths that must stay silent (the
/// offline schema validators re-render the same artifact without logging).
///
/// Moved here so all publishers share a single implementation; previously
/// homebrew.rs owned this and other crates reached across modules.
pub(crate) fn should_skip_upload(
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    ctx: &Context,
    log: &StageLogger,
    label: Option<&str>,
) -> Result<bool> {
    let raw = match skip_upload {
        Some(v) => v.as_str(),
        None => return Ok(false),
    };
    let rendered = super::template::render_or_warn(ctx, log, "skip_upload", raw)?;
    let skip = match rendered.trim() {
        "true" => true,
        "auto" => {
            let pre = ctx
                .template_vars()
                .get("Prerelease")
                .cloned()
                .unwrap_or_default();
            !pre.is_empty()
        }
        "false" | "" => false,
        other => {
            log.warn(&format!(
                "unrecognized skip_upload value {other:?} (expected \"true\", \"false\", or \"auto\"); treating as false"
            ));
            false
        }
    };
    if skip && let Some(label) = label {
        log.status(&format!("skipped {label} upload — skip_upload is set"));
    }
    Ok(skip)
}

/// Evaluate `skip` / `skip_upload` / `if:` fields for a publisher entry.
///
/// Returns `Ok(true)` to skip; `Ok(false)` to proceed. Consolidates the
/// per-publisher pattern of near-identical `if let Some(d) = ...` blocks
/// that previously lived in aur_source.rs (per-crate AND top-level paths),
/// extended to also evaluate the `if:` conditional gate.
/// `if_condition` is the resource's `if:` template (or `None` when the
/// resource does not yet expose `if:`).
pub(crate) fn should_skip_publisher_with_if(
    ctx: &Context,
    skip: Option<&anodizer_core::config::StringOrBool>,
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    if_condition: Option<&str>,
    label: &str,
    log: &StageLogger,
) -> Result<bool> {
    if let Some(d) = skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("{label}: render skip template"))?;
        if off {
            log.status(&format!(
                "skipped {label} — `skip` condition evaluated truthy"
            ));
            return Ok(true);
        }
    }
    // `skip_upload` honors the `auto` value (skip for prereleases) via the
    // shared `should_skip_upload`, NOT a bare `try_evaluates_to_true` — a
    // bare bool-eval would silently treat `auto` as an unknown string and
    // never skip a prerelease, regressing the documented `skip_upload: auto`
    // semantics.
    if skip_upload.is_some() && should_skip_upload(skip_upload, ctx, log, Some(label))? {
        return Ok(true);
    }
    let proceed = anodizer_core::config::evaluate_if_condition(if_condition, label, |t| {
        ctx.render_template(t)
    })?;
    if !proceed {
        log.status(&format!("skipped {label} — `if` condition evaluated falsy"));
        return Ok(true);
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Repository token resolution
// ---------------------------------------------------------------------------

/// Resolve the repository token from: RepositoryConfig.token -> env_var -> ANODIZER_GITHUB_TOKEN -> GITHUB_TOKEN.
pub(crate) fn resolve_repo_token(
    ctx: &Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    env_var: Option<&str>,
) -> Option<String> {
    // 1. Token from repository config. May be templated
    // (`token: "{{ .Env.GH_PAT }}"`); render before it is used as the
    // bearer / x-access-token credential, or the literal template string
    // is sent as the auth token.
    if let Some(r) = repo
        && let Some(ref tok) = r.token
        && !tok.is_empty()
    {
        let rendered = ctx.render_template(tok).unwrap_or_else(|_| tok.clone());
        if !rendered.is_empty() {
            return Some(rendered);
        }
    }
    // 2. Fall back to context + env
    resolve_token(ctx, env_var)
}

/// Resolve a GitHub token for a *rollback* target straight from an
/// [`EnvSource`](anodizer_core::EnvSource) (rollback runs from a persisted
/// target snapshot, not a live `Context`).
///
/// Precedence: the target's custom `token_env_var` (when it names a non-empty
/// var) → `ANODIZER_GITHUB_TOKEN` → `GITHUB_TOKEN`. Empty values are skipped at
/// every link because it delegates to the canonical
/// [`resolve_github_token_with_env`](anodizer_core::git::resolve_github_token_with_env)
/// — a `GITHUB_TOKEN=""` (the shape GitHub Actions materializes for a missing
/// secret) must not be treated as a real token.
pub(crate) fn resolve_rollback_token<E: anodizer_core::EnvSource + ?Sized>(
    env: &E,
    token_env_var: Option<&str>,
) -> Option<String> {
    let explicit = token_env_var.and_then(|n| env.var(n));
    anodizer_core::git::resolve_github_token_with_env(explicit.as_deref(), &|key| env.var(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::config::{
        BinstallConfig, CrateConfig, NixConfig, PublishConfig, RepositoryConfig, WorkspaceConfig,
    };
    use anodizer_core::test_helpers::TestContextBuilder;

    /// `resolve_rollback_token` must empty-filter every link: a set-but-blank
    /// `GITHUB_TOKEN=""` (GitHub Actions' shape for a missing secret) falls
    /// through to the next source rather than masquerading as a real token.
    #[test]
    fn rollback_token_skips_empty_github_token() {
        let env = MapEnvSource::new()
            .with("GITHUB_TOKEN", "")
            .with("ANODIZER_GITHUB_TOKEN", "real");
        assert_eq!(resolve_rollback_token(&env, None).as_deref(), Some("real"));
    }

    /// A custom `token_env_var` that is set-but-empty also falls through to the
    /// standard chain instead of returning `""`.
    #[test]
    fn rollback_token_empty_custom_var_falls_through() {
        let env = MapEnvSource::new()
            .with("MY_TOKEN", "")
            .with("GITHUB_TOKEN", "gh");
        assert_eq!(
            resolve_rollback_token(&env, Some("MY_TOKEN")).as_deref(),
            Some("gh")
        );
    }

    /// A populated custom `token_env_var` wins over the standard chain.
    #[test]
    fn rollback_token_custom_var_wins_when_set() {
        let env = MapEnvSource::new()
            .with("MY_TOKEN", "custom")
            .with("GITHUB_TOKEN", "gh");
        assert_eq!(
            resolve_rollback_token(&env, Some("MY_TOKEN")).as_deref(),
            Some("custom")
        );
    }

    /// All sources empty/absent resolves to `None` (no `""` token leaks out).
    #[test]
    fn rollback_token_none_when_all_empty() {
        let env = MapEnvSource::new().with("GITHUB_TOKEN", "");
        assert_eq!(resolve_rollback_token(&env, None), None);
    }

    /// A templated `repository.token` (`{{ .Env.GH_PAT }}`) must be rendered
    /// to the env value before it is used as the auth credential — the
    /// literal template string must never become the bearer token.
    #[test]
    fn resolve_repo_token_renders_templated_token() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut().set_env("GH_PAT", "ghp_real_value");
        let repo = RepositoryConfig {
            token: Some("{{ .Env.GH_PAT }}".into()),
            ..Default::default()
        };
        let tok = resolve_repo_token(&ctx, Some(&repo), None);
        assert_eq!(tok.as_deref(), Some("ghp_real_value"));
        assert!(
            !tok.unwrap().contains("{{"),
            "the literal template must never become the auth token"
        );
    }

    /// A templated `secret_name` (`TOKEN_{{ .Env.X }}`) must render to the
    /// env-var name BEFORE the lookup, identically regardless of which
    /// publisher's default is supplied. cloudsmith already routed through this
    /// SSOT; dockerhub and gemfury now do too, so a `secret_name:
    /// "TOKEN_{{ .Env.X }}"` with `.Env.X=FOO` resolves to `TOKEN_FOO` for all
    /// three rather than being looked up as the literal template string.
    #[test]
    fn resolve_secret_name_renders_templated_value_identically_across_publishers() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut().set_env("X", "FOO");
        let templated = Some("TOKEN_{{ .Env.X }}");
        // The default differs per publisher but the render path is the same;
        // a present (templated) `secret_name` ignores the default entirely.
        let cloudsmith = resolve_secret_name(&ctx, templated, "CLOUDSMITH_TOKEN");
        let dockerhub = resolve_secret_name(&ctx, templated, "DOCKER_PASSWORD");
        let gemfury = resolve_secret_name(&ctx, templated, "FURY_PUSH_TOKEN");
        assert_eq!(cloudsmith, "TOKEN_FOO");
        assert_eq!(dockerhub, "TOKEN_FOO");
        assert_eq!(gemfury, "TOKEN_FOO");
        assert!(
            !cloudsmith.contains("{{"),
            "the literal template must never become the env-var name"
        );
    }

    /// A plain (non-templated) `repository.token` passes through unchanged.
    #[test]
    fn resolve_repo_token_returns_plain_token_verbatim() {
        let ctx = TestContextBuilder::new().build();
        let repo = RepositoryConfig {
            token: Some("ghp_literal".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_repo_token(&ctx, Some(&repo), None).as_deref(),
            Some("ghp_literal")
        );
    }

    /// A crate carrying a `publish.nix` block plus an enabled binstall
    /// emission — the shape `get_publish_config` and the snapshot validator
    /// must resolve by name.
    fn nix_binstall_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            binstall: Some(BinstallConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                nix: Some(NixConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn get_publish_config_resolves_workspace_only_crate() {
        // A pure-workspace config (no top-level `crates:`) is cfgd's shape:
        // every crate lives under `workspaces:`. Before the universe-aware
        // lookup, `get_publish_config` searched only `ctx.config.crates`
        // (empty here) and bailed "crate 'cfgd' not found in config",
        // breaking emission-validate for nix/binstall/homebrew/scoop/...
        let ctx = TestContextBuilder::new()
            .workspaces(vec![WorkspaceConfig {
                name: "cfgd".to_string(),
                crates: vec![nix_binstall_crate("cfgd")],
                ..Default::default()
            }])
            .build();

        let (crate_cfg, publish) =
            get_publish_config(&ctx, "cfgd", "nix").expect("workspace crate must resolve by name");
        assert_eq!(crate_cfg.name, "cfgd");
        assert!(publish.nix.is_some(), "nix block must be reachable");
        assert!(
            crate_cfg
                .binstall
                .as_ref()
                .is_some_and(|b| b.enabled == Some(true)),
            "binstall block must be reachable on the resolved crate"
        );
    }

    #[test]
    fn get_publish_config_top_level_wins_over_workspace_on_name_collision() {
        // Mirrors `crate_universe` precedence: a top-level entry shadows a
        // workspace entry sharing its name.
        let mut top = nix_binstall_crate("dup");
        top.path = "crates/top".to_string();
        let mut ws = nix_binstall_crate("dup");
        ws.path = "ws/dup".to_string();
        let ctx = TestContextBuilder::new()
            .crates(vec![top])
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![ws],
                ..Default::default()
            }])
            .build();

        let (crate_cfg, _publish) =
            get_publish_config(&ctx, "dup", "nix").expect("collision must resolve");
        assert_eq!(
            crate_cfg.path, "crates/top",
            "top-level entry must win on name collision"
        );
    }

    #[test]
    fn get_publish_config_resolves_crate_in_a_later_workspace() {
        // Multi-workspace monorepo: the target crate lives in the SECOND
        // workspace. The lookup flat_maps across every `workspaces[]` entry,
        // so a crate is found regardless of which workspace declares it — a
        // search that stopped at `workspaces[0]` would miss it here.
        let ctx = TestContextBuilder::new()
            .workspaces(vec![
                WorkspaceConfig {
                    name: "first".to_string(),
                    crates: vec![nix_binstall_crate("alpha")],
                    ..Default::default()
                },
                WorkspaceConfig {
                    name: "second".to_string(),
                    crates: vec![nix_binstall_crate("omega")],
                    ..Default::default()
                },
            ])
            .build();

        let (crate_cfg, publish) = get_publish_config(&ctx, "omega", "nix")
            .expect("crate in a later workspace must resolve by name");
        assert_eq!(crate_cfg.name, "omega");
        assert!(publish.nix.is_some(), "nix block must be reachable");
    }

    #[test]
    fn get_publish_config_unknown_crate_still_errors() {
        let ctx = TestContextBuilder::new()
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![nix_binstall_crate("present")],
                ..Default::default()
            }])
            .build();
        let err = get_publish_config(&ctx, "absent", "nix").expect_err("must error");
        assert!(
            err.to_string().contains("not found in config"),
            "missing crate must still bail: {err}"
        );
    }
}
