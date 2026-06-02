//! Config / context lookups & resolution helpers shared across publishers.
//!
//! - Crate-universe walker (`all_crates`).
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
///
/// On a name collision where the colliding entries point at different
/// `path` values (almost certainly a config mistake — two distinct crates
/// sharing a name), emit a warning so the operator notices the dropped
/// workspace entry. The dedup itself stays silent for the legitimate
/// case (the same crate referenced from both top-level and a workspace).
pub(crate) fn all_crates(ctx: &Context) -> Vec<CrateConfig> {
    let mut acc = ctx.config.crates.clone();
    if let Some(ref ws_list) = ctx.config.workspaces {
        let log = ctx.logger("publish");
        for ws in ws_list {
            for c in &ws.crates {
                if let Some(existing) = acc.iter().find(|e| e.name == c.name) {
                    if existing.path != c.path {
                        log.warn(&format!(
                            "all_crates: workspace '{}' crate '{}' path '{}' shadowed by \
                             prior entry with path '{}'; workspace entry dropped (name \
                             collision with different paths — likely a config mistake)",
                            ws.name, c.name, c.path, existing.path
                        ));
                    }
                    continue;
                }
                acc.push(c.clone());
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
///
/// Resolves against the full crate universe — top-level `ctx.config.crates`
/// PLUS every `ctx.config.workspaces[].crates` — so a workspace-only crate
/// (the only shape in a pure-workspace config like a multi-crate monorepo)
/// is found by name. Top-level entries take precedence on a name collision,
/// matching [`all_crates`]'s dedup order. Without the workspace fallthrough
/// every per-publisher lookup (nix, homebrew, scoop, aur, krew, winget,
/// chocolatey) — and the snapshot emission validator that drives them —
/// would `bail!` "not found" for a crate defined under `workspaces:`.
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

/// Borrow a crate by name from the full crate universe: `ctx.config.crates`
/// first (top-level wins, mirroring [`all_crates`] dedup precedence), then
/// the first matching `ctx.config.workspaces[].crates` entry. Returns a
/// reference tied to `ctx` so callers can keep zero-copy `&CrateConfig`
/// access without cloning the whole universe.
fn find_crate_in_universe<'a>(ctx: &'a Context, crate_name: &str) -> Option<&'a CrateConfig> {
    if let Some(c) = ctx.config.crates.iter().find(|c| c.name == crate_name) {
        return Some(c);
    }
    ctx.config
        .workspaces
        .as_ref()?
        .iter()
        .flat_map(|ws| ws.crates.iter())
        .find(|c| c.name == crate_name)
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
        .or_else(|| ctx.env_var("ANODIZER_GITHUB_TOKEN").and_then(non_empty))
        .or_else(|| ctx.env_var("GITHUB_TOKEN").and_then(non_empty))
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
            log.status(&format!("{label}: skipped"));
            return Ok(true);
        }
    }
    // `skip_upload` honors the `auto` value (skip for prereleases) via the
    // shared `should_skip_upload`, NOT a bare `try_evaluates_to_true` — a
    // bare bool-eval would silently treat `auto` as an unknown string and
    // never skip a prerelease, regressing the documented `skip_upload: auto`
    // semantics.
    if skip_upload.is_some() && should_skip_upload(skip_upload, ctx, log) {
        log.status(&format!("{label}: skipping upload (skip_upload)"));
        return Ok(true);
    }
    let proceed = anodizer_core::config::evaluate_if_condition(if_condition, label, |t| {
        ctx.render_template(t)
    })?;
    if !proceed {
        log.status(&format!(
            "{label}: skipped — `if` condition evaluated falsy"
        ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        BinstallConfig, CrateConfig, NixConfig, PublishConfig, WorkspaceConfig,
    };
    use anodizer_core::log::LogCapture;
    use anodizer_core::test_helpers::TestContextBuilder;

    fn crate_with(name: &str, path: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }
    }

    /// A crate carrying a `publish.nix` block plus an enabled binstall
    /// emission — the shape `get_publish_config` and the snapshot validator
    /// must resolve by name.
    fn nix_binstall_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
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
        // Mirrors `all_crates` precedence: a top-level entry shadows a
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

    #[test]
    fn all_crates_dedups_silently_when_paths_match() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with("foo", ".")])
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![crate_with("foo", ".")],
                ..Default::default()
            }])
            .build();
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());

        let out = all_crates(&ctx);
        assert_eq!(out.len(), 1, "dedup keeps one entry: {:?}", out);
        assert_eq!(cap.warn_count(), 0, "same-path dedup must stay silent");
    }

    #[test]
    fn all_crates_warns_when_name_collides_with_different_paths() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with("foo", "crates/foo")])
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![crate_with("foo", "other/path/foo")],
                ..Default::default()
            }])
            .build();
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());

        let out = all_crates(&ctx);
        assert_eq!(out.len(), 1, "workspace entry must be dropped");
        assert_eq!(out[0].path, "crates/foo", "top-level entry wins");
        assert_eq!(
            cap.warn_count(),
            1,
            "operator must be warned about the dropped workspace entry"
        );
        let msgs = cap.all_messages();
        let warn_msg = &msgs[0].1;
        assert!(
            warn_msg.contains("foo"),
            "warn must name the crate: {warn_msg}"
        );
        assert!(
            warn_msg.contains("ws-a"),
            "warn must name the workspace: {warn_msg}"
        );
        assert!(
            warn_msg.contains("other/path/foo"),
            "warn must show the dropped path: {warn_msg}"
        );
    }
}
