use anodizer_core::config::{Config, WorkspaceConfig};
use anodizer_core::log::StageLogger;
use anyhow::Result;

/// Collect all configured build targets from a config, in declaration order.
///
/// Iterates `config.crates` plus every `config.workspaces[].crates` so monorepos
/// with multi-root workspaces are covered. Per-crate `builds[].targets` entries
/// REPLACE `defaults.targets` for that build (override semantics — matching
/// the `BuildConfig.targets` rustdoc and the stage-build runtime). Builds
/// whose `targets` field is `None` fall back to `defaults.targets`.
/// Duplicates are filtered across all builds, and `defaults.builds.ignore`
/// (os/arch pairs) removes matching targets.
///
/// `selected_crates` filters the iteration: when empty, all crates are used;
/// otherwise only crates whose `name` is in the slice contribute.
pub fn collect_build_targets(config: &Config, selected_crates: &[String]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    // SSOT for the unset fallback: the canonical DEFAULT_TARGETS set the build
    // planner uses, not an empty list. A synthesized default build (a crate
    // with no `builds:` but a declared `--bin`) compiles over this set, so the
    // host filter / `anodizer targets` must see it too.
    let default_targets = config.effective_default_targets();

    for krate in config.crate_universe() {
        if !selected_crates.is_empty() && !selected_crates.contains(&krate.name) {
            continue;
        }

        // Enumerate exactly what the planner compiles for this crate via the
        // shared SSOT: a non-empty `builds:` list as-is, else a synthesized
        // default build when the crate declares a `--bin <name>`, else nothing.
        // The compile/artifact gate inside `crate_target_list` drops a library
        // crate with no default binary — it builds nothing, so reporting
        // `defaults.targets` for it would over-report what the build produces.
        for t in anodizer_core::build_plan::crate_target_list(krate, &default_targets) {
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
    }

    if let Some(ignores) = config
        .defaults
        .as_ref()
        .and_then(|d| d.builds.as_ref())
        .and_then(|b| b.ignore.as_ref())
    {
        targets.retain(|t| {
            let (os, arch) = anodizer_core::target::map_target(t);
            !ignores.iter().any(|ig| ig.os == os && ig.arch == arch)
        });
    }

    targets
}

/// Apply a workspace's configuration overlay onto the top-level config.
///
/// - `crates` is always replaced; `workspaces` is always cleared.
/// - `changelog`, `signs`, `before`, and `after` replace when present.
/// - `env` is merged additively (workspace values override same-key top-level values).
pub fn apply_workspace_overlay(config: &mut Config, ws: &WorkspaceConfig) {
    config.crates = ws.crates.clone();
    // The overlaid run IS this workspace: its crates are now the top-level
    // list, so the sibling workspaces must not stay visible. Leaving them in
    // place would put every sibling crate back into `crate_universe()`, and
    // the stages' "empty selection = all" walks would build/publish sibling
    // crates under THIS workspace's env/signs/skip.
    config.workspaces = None;
    if ws.changelog.is_some() {
        config.changelog = ws.changelog.clone();
    }
    if !ws.signs.is_empty() {
        config.signs = ws.signs.clone();
    }
    if !ws.binary_signs.is_empty() {
        config.binary_signs = ws.binary_signs.clone();
    }
    if ws.before.is_some() {
        config.before = ws.before.clone();
    }
    if ws.after.is_some() {
        config.after = ws.after.clone();
    }
    if let Some(ref env_list) = ws.env {
        let merged = config.env.get_or_insert_with(Vec::new);
        merged.extend(env_list.iter().cloned());
    }
}

/// The workspace whose `crates:` list declares `name`, or `None` when the
/// name is a top-level crate (the universe's first-seen shadowing — a
/// top-level entry wins over a same-named workspace entry) or is unknown.
///
/// The one lookup every workspace-overlay inference resolves through
/// (release's `--crate` selection, changelog's `--crate` filter), so the
/// shadowing rule cannot drift between commands.
pub fn workspace_containing_crate<'a>(
    config: &'a Config,
    name: &str,
) -> Option<&'a WorkspaceConfig> {
    if config.crates.iter().any(|c| c.name == name) {
        return None;
    }
    config
        .workspaces
        .iter()
        .flatten()
        .find(|ws| ws.crates.iter().any(|c| c.name == name))
}

/// Resolve a workspace by name from the config. Returns an error if
/// `workspaces` is not configured or the given name is not found.
pub fn resolve_workspace<'a>(config: &'a Config, name: &str) -> Result<&'a WorkspaceConfig> {
    let workspaces = config.workspaces.as_ref().ok_or_else(|| {
        anyhow::anyhow!("--workspace specified but no workspaces defined in config")
    })?;

    workspaces.iter().find(|ws| ws.name == name).ok_or_else(|| {
        let available: Vec<&str> = workspaces.iter().map(|ws| ws.name.as_str()).collect();
        anyhow::anyhow!(
            "workspace '{}' not found (available: {})",
            name,
            available.join(", ")
        )
    })
}

/// Apply the workspace scope for a command run: the explicit `--workspace`
/// overlay, or the one inferred from the `--crate` selection when it resolves
/// into a single workspace. Returns the workspace-level skip stages to merge
/// into the run's skip list.
///
/// After any overlay decision, every explicitly-selected crate name is
/// validated against the post-overlay universe: the topo sort and the stages'
/// crate filters silently drop unknown names, and several run modes treat an
/// empty selection as "all crates", so an unmatched name would otherwise flip
/// a scoped request into a broader (or empty) run instead of failing loudly.
pub fn apply_workspace_scope(
    config: &mut Config,
    workspace: Option<&str>,
    crate_names: &[String],
    log: &StageLogger,
) -> Result<Vec<String>> {
    let mut workspace_skip: Vec<String> = Vec::new();
    let mut applied_ws: Option<String> = workspace.map(str::to_string);
    if let Some(ws_name) = workspace {
        let ws = resolve_workspace(config, ws_name)?.clone();
        workspace_skip = ws.skip.clone();
        apply_workspace_overlay(config, &ws);
    } else if let Some(ws_name) = infer_workspace_for_selection(config, crate_names)? {
        // No --workspace given, but the whole --crate selection lives in one
        // workspace — apply its overlay so the crates' workspace-level
        // context (skip/env/signs) applies. Matches user intuition:
        // "release crate X" should release X under X's workspace settings.
        log.verbose(&format!(
            "--crate selection lives in workspace '{}'; applying workspace overlay",
            ws_name
        ));
        let ws = resolve_workspace(config, &ws_name)?.clone();
        workspace_skip = ws.skip.clone();
        apply_workspace_overlay(config, &ws);
        applied_ws = Some(ws_name);
    }
    validate_selection_against_universe(config, crate_names, applied_ws.as_deref())?;
    Ok(workspace_skip)
}

/// Resolve which workspace (if any) an explicit `--crate` selection infers.
///
/// The decision considers EVERY selected name, not just the first: the
/// overlay replaces the crate universe and applies one workspace's
/// env/signs/skip to the whole run, so a selection spanning a workspace and
/// top-level crates (or two workspaces) has no single correct overlay — some
/// crates would release under another scope's settings, or fall out of the
/// post-overlay universe entirely. Such a selection is a hard error naming
/// each crate and its home.
///
/// A name that is a top-level crate counts as top-level even when a workspace
/// declares the same name (the universe's first-seen shadowing). Names found
/// nowhere are ignored here — the post-overlay universe validation rejects
/// them with the right scope context.
pub fn infer_workspace_for_selection(
    config: &Config,
    crate_names: &[String],
) -> Result<Option<String>> {
    if crate_names.is_empty() {
        return Ok(None);
    }
    let mut homes: Vec<(String, String)> = Vec::new();
    let mut ws_names: Vec<String> = Vec::new();
    let mut has_top_level = false;
    for name in crate_names {
        if config.crates.iter().any(|c| &c.name == name) {
            has_top_level = true;
            homes.push((name.clone(), "top-level".to_string()));
        } else if let Some(ws) = workspace_containing_crate(config, name) {
            if !ws_names.contains(&ws.name) {
                ws_names.push(ws.name.clone());
            }
            homes.push((name.clone(), format!("workspace '{}'", ws.name)));
        }
    }
    if ws_names.is_empty() {
        return Ok(None);
    }
    if has_top_level || ws_names.len() > 1 {
        let listing = homes
            .iter()
            .map(|(name, home)| format!("'{name}' ({home})"))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "--crate selection spans multiple release scopes: {listing}. One run applies a \
             single workspace's overlay (env/signs/skip), so a mixed selection cannot release \
             every named crate correctly — select crates from one scope per run (or pass \
             --workspace <name>)"
        );
    }
    Ok(Some(ws_names.remove(0)))
}

/// Reject any explicitly-selected crate name absent from the post-overlay
/// crate universe. `scope` names the workspace whose overlay was applied (so
/// the error can say WHY the crate is out of reach), or `None` when no
/// overlay ran.
pub fn validate_selection_against_universe(
    config: &Config,
    crate_names: &[String],
    scope: Option<&str>,
) -> Result<()> {
    let universe: Vec<&str> = config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.as_str())
        .collect();
    let unknown: Vec<&str> = crate_names
        .iter()
        .map(|n| n.as_str())
        .filter(|n| !universe.contains(n))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let known = if universe.is_empty() {
        "(none)".to_string()
    } else {
        universe.join(", ")
    };
    match scope {
        Some(ws) => anyhow::bail!(
            "--crate {}: not in workspace '{}' (its crates: {}); a workspace-scoped run \
             releases only that workspace's crates",
            unknown.join(", "),
            ws,
            known
        ),
        // An empty universe means NO name could ever validate, so "known
        // crates: (none)" would state the problem without a way out — name
        // the two exits instead.
        None if universe.is_empty() => anyhow::bail!(
            "--crate {}: the configuration defines no crates; drop --crate to run at the \
             repo level, or add a `crates:` entry for '{}'",
            unknown.join(", "),
            unknown.join(", ")
        ),
        None => anyhow::bail!(
            "--crate {}: no such crate in the configuration (known crates: {})",
            unknown.join(", "),
            known
        ),
    }
}

/// Append every stage in `extra` to `skip_stages`, skipping names already
/// present. The one merge used everywhere a workspace-implied (or
/// mode-implied) skip list joins the CLI's `--skip` set, so the dedup
/// semantics cannot drift between commands.
pub fn merge_skip_stages<S: AsRef<str>>(skip_stages: &mut Vec<String>, extra: &[S]) {
    for stage in extra {
        let stage = stage.as_ref();
        if !skip_stages.iter().any(|s| s == stage) {
            skip_stages.push(stage.to_string());
        }
    }
}
