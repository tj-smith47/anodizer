//! Config / context lookups & resolution helpers shared across publishers.
//!
//! - Crate-universe walker (`all_crates`).
//! - Publisher config lookup (`get_publish_config`).
//! - Artifact-kind resolution (`resolve_artifact_kind`).
//! - Token / secret resolution (`resolve_token`, `resolve_repo_token`,
//!   `resolve_secret_name`).
//! - Repository owner/name extraction (`resolve_repo_owner_name`).
//! - Skip-gate evaluation (`should_skip_upload`, `should_skip_publisher`).

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{CrateConfig, PublishConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Crate universe walker (shared across publisher dispatch + cargo flatten)
// ---------------------------------------------------------------------------

/// Flatten `ctx.config.crates` plus every `ctx.config.workspaces[].crates`
/// into a single de-duplicated `Vec<CrateConfig>` (dedup by `name`,
/// `ctx.config.crates` wins on collision). This is the universe of crates
/// every per-crate publisher must walk — without this, a workspace-only
/// crate carrying a non-cargo publisher block (homebrew, scoop, ...) is
/// invisible because the dispatcher only looks at `ctx.config.crates`,
/// while `cargo.rs` flattens both. The cargo + non-cargo walkers must
/// share one universe so a crate with both `cargo:` and `homebrew:` is
/// either eligible everywhere or skipped everywhere.
pub(crate) fn all_crates(ctx: &Context) -> Vec<CrateConfig> {
    let mut acc = ctx.config.crates.clone();
    if let Some(ref ws_list) = ctx.config.workspaces {
        for ws in ws_list {
            for c in &ws.crates {
                if !acc.iter().any(|existing| existing.name == c.name) {
                    acc.push(c.clone());
                }
            }
        }
    }
    acc
}

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
pub(crate) fn get_publish_config<'a>(
    ctx: &'a Context,
    crate_name: &str,
    label: &str,
) -> Result<(&'a CrateConfig, &'a PublishConfig)> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("{label}: crate '{crate_name}' not found in config"))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{label}: no publish config for '{crate_name}'"))?;

    Ok((crate_cfg, publish))
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
pub(crate) fn resolve_token(ctx: &Context, env_var: Option<&str>) -> Option<String> {
    // Filter empty strings: GitHub Actions sets env vars from non-existent
    // secrets to "", which would short-circuit the fallback chain and prevent
    // GITHUB_TOKEN from being used.
    let non_empty = |s: String| if s.is_empty() { None } else { Some(s) };
    ctx.options
        .token
        .clone()
        .and_then(non_empty)
        .or_else(|| {
            env_var
                .and_then(|v| std::env::var(v).ok())
                .and_then(non_empty)
        })
        .or_else(|| {
            std::env::var("ANODIZER_GITHUB_TOKEN")
                .ok()
                .and_then(non_empty)
        })
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().and_then(non_empty))
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

/// Resolve `skip_upload` to a boolean for publisher entry-points.
///
/// Accepts the StringOrBool field directly. Templates are rendered via
/// `ctx.render_template`. Returns:
/// - `true` if the value renders to literal "true", or to "auto" while the
///   release is a prerelease (template var `Prerelease` is non-empty).
/// - `false` if `None`, or renders to "false"/"".
/// - `false` with a warn if the value is an unrecognized string.
///
/// Moved here so all publishers share a single implementation; previously
/// homebrew.rs owned this and other crates reached across modules.
pub(crate) fn should_skip_upload(
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    ctx: &Context,
    log: &StageLogger,
) -> bool {
    let raw = match skip_upload {
        Some(v) => v.as_str(),
        None => return false,
    };
    let rendered = ctx.render_template(raw).unwrap_or_else(|_| raw.to_string());
    match rendered.trim() {
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
    }
}

/// Evaluate `skip` / `skip_upload` fields for a publisher entry.
///
/// Returns `Ok(true)` to skip; `Ok(false)` to proceed. This consolidates the
/// per-publisher pattern of two near-identical `if let Some(d) = ...` blocks
/// that previously lived in aur_source.rs (per-crate AND top-level paths).
pub(crate) fn should_skip_publisher(
    ctx: &Context,
    skip: Option<&anodizer_core::config::StringOrBool>,
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    label: &str,
    log: &StageLogger,
) -> Result<bool> {
    if let Some(d) = skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("{label}: render skip template"))?;
        if off {
            log.status(&format!("{label}: skipped"));
            return Ok(true);
        }
    }
    if let Some(s) = skip_upload {
        let off = s
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("{label}: render skip_upload template"))?;
        if off {
            log.status(&format!("{label}: skipping upload (skip_upload=true)"));
            return Ok(true);
        }
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
    // 1. Token from repository config
    if let Some(r) = repo
        && let Some(ref tok) = r.token
        && !tok.is_empty()
    {
        return Some(tok.clone());
    }
    // 2. Fall back to context + env
    resolve_token(ctx, env_var)
}
