//! Building the cargo publish plan: publish order, per-crate configs, and
//! binstall metadata.

use super::*;

/// The eligible cargo-publish set, resolved once and shared between the
/// real publisher and the publish-simulation preflight.
///
/// Holds everything both consumers need so the topological/eligibility
/// derivation lives in exactly one place:
/// - `order` — crate names in dependency-first publish order.
/// - `cfgs` — per-crate resolved `publish.cargo` block (post `skip:`/`if:`).
/// - `versions` — per-crate resolved version (each crate's own Cargo.toml
///   `[package].version`, falling back to the release version), since
///   mixed-cadence workspaces publish different versions per crate.
/// - `all_crates` — the full crate universe (top-level + workspace overlay)
///   the plan was derived from, reused by callers that need `depends_on`.
pub(crate) struct CargoPublishPlan {
    pub order: Vec<String>,
    pub cfgs: HashMap<String, CargoPublishConfig>,
    pub versions: HashMap<String, String>,
    pub all_crates: Vec<CrateConfig>,
}

/// Resolve the cargo-publish set: the crates that a real release WOULD
/// publish at their target versions, in dependency-first order.
///
/// Reuses the exact eligibility rules the publisher applies — `publish.cargo`
/// presence, the peer `skip:` template, the `if:` condition, and the
/// `--crate` selection (expanded transitively via `expand_with_transitive_deps`)
/// — then orders the survivors with [`topological_sort`]. This is the single
/// source of truth for "what would be published"; the publish-simulation
/// preflight and [`publish_to_cargo_with`] both consume it so they can never
/// disagree about the set or its order.
///
/// `log` receives the same per-crate `skip:`/`if:` status lines the publisher
/// emits, so resolving the plan twice (preflight + publish) is idempotent in
/// behaviour but produces those lines once per resolution; callers that only
/// want the set (the preflight) pass a quiet/verbose logger.
pub(crate) fn cargo_publish_plan(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
) -> Result<CargoPublishPlan> {
    let all_crates: Vec<CrateConfig> = ctx.config.crate_universe().into_iter().cloned().collect();

    let expanded_selection: Vec<String> = if selected.is_empty() {
        Vec::new()
    } else {
        expand_with_transitive_deps(&all_crates, selected)
    };
    let selected_set: std::collections::HashSet<&str> =
        expanded_selection.iter().map(|s| s.as_str()).collect();

    let cfgs: HashMap<String, CargoPublishConfig> = {
        let mut m = HashMap::new();
        for c in &all_crates {
            let Some(ref publish) = c.publish else {
                continue;
            };
            let Some(ref cargo_cfg) = publish.cargo else {
                continue;
            };
            if let Some(ref d) = cargo_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| format!("cargo: render skip template for '{}'", c.name))?;
                if off {
                    log.status(&format!(
                        "skipped cargo publish for '{}' — skip=true",
                        c.name
                    ));
                    continue;
                }
            }
            let proceed = anodizer_core::config::evaluate_if_condition(
                cargo_cfg.if_condition.as_deref(),
                &format!("cargo publisher for crate '{}'", c.name),
                |t| ctx.render_template(t),
            )?;
            if !proceed {
                log.status(&format!(
                    "skipped cargo publish for '{}' — `if` condition evaluated falsy",
                    c.name
                ));
                continue;
            }
            m.insert(c.name.clone(), cargo_cfg.clone());
        }
        m
    };

    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| cfgs.contains_key(&c.name))
        .map(|c| {
            let deps = c.depends_on.clone().unwrap_or_default();
            (c.name.clone(), deps)
        })
        .collect();

    let order = topological_sort(&publishable);

    let versions: HashMap<String, String> = all_crates
        .iter()
        .filter(|c| order.iter().any(|n| n == &c.name))
        .map(|c| {
            // Use an empty string when the per-crate manifest is unreadable so
            // the skip-decision treats the crate as "not yet published" (safe
            // path). Falling back to the global release version here would key
            // the idempotency probe on the WRONG version in per-crate workspaces
            // and cause the crate's real version to be silently skipped.
            let v = read_cargo_toml_version(&c.path).unwrap_or_default();
            (c.name.clone(), v)
        })
        .collect();

    Ok(CargoPublishPlan {
        order,
        cfgs,
        versions,
        all_crates,
    })
}

/// Resolve the project-wide `default_targets` the build stage would use:
/// `defaults.targets` when non-empty, else the canonical default matrix.
///
/// Routed through `Config::effective_default_targets` — the same helper the
/// build stage uses — so the binstall override set the cargo publisher emits
/// equals the one the build stage emits for the same config; any divergence
/// would surface as a per-target asset mismatch between the two paths.
pub(crate) fn resolve_default_targets(ctx: &Context) -> Vec<String> {
    ctx.config.effective_default_targets()
}

/// Guarantee `[package.metadata.binstall]` is present and current in
/// `crate_cfg`'s on-disk `Cargo.toml` immediately before `cargo publish`.
///
/// Binstall metadata is a *published-manifest* property: `cargo binstall`
/// reads it from the manifest on crates.io to fetch a prebuilt asset instead
/// of compiling from source. The build stage emits it too, but the real
/// release runs `anodizer release --publish-only`, which consumes preserved
/// dist artifacts and skips the build stage entirely — so without this call the
/// published manifest carries no binstall metadata and `cargo binstall`
/// silently falls back to a source compile.
///
/// The emitter is idempotent (it re-writes only the keys it owns and preserves
/// user-authored ones), so invoking it here when the build stage already ran in
/// the full pipeline is a safe no-op-equivalent rewrite, not a double-write
/// divergence. Per-crate template vars are re-scoped via [`with_crate_scope`]
/// exactly as the build stage does, so the emitted overrides are byte-identical
/// across the two paths in single-crate, workspace-lockstep, and workspace
/// per-crate modes.
///
/// Honors `dry_run` (the emitter does not mutate under dry-run); the caller
/// already early-returns before the publish loop on `ctx.is_dry_run()`, so in
/// practice this only runs on a real publish.
/// The per-crate tag source is injected. The publish loop passes
/// [`anodizer_core::crate_scope::resolve_crate_tag`] (git-backed, threaded in
/// from the public entry points); tests pass a closure returning a fixed tag so
/// the per-crate var scoping — and the resulting override set — can be exercised
/// without a git fixture. Mirrors the build stage's
/// `apply_source_mutations_with_resolver`
/// seam so both paths are testable the same way.
pub(crate) fn ensure_binstall_metadata_with(
    ctx: &mut Context,
    crate_cfg: &CrateConfig,
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    let Some(ref binstall_cfg) = crate_cfg.binstall else {
        return Ok(());
    };
    if !binstall_cfg.enabled.unwrap_or(false) {
        return Ok(());
    }
    let default_targets = resolve_default_targets(ctx);
    let binstall_cfg = binstall_cfg.clone();
    anodizer_core::crate_scope::with_crate_scope(ctx, crate_cfg, resolve_tag, |ctx| {
        anodizer_core::binstall::generate_binstall_metadata(
            crate_cfg,
            &binstall_cfg,
            &default_targets,
            ctx,
            dry_run,
        )
    })
    .with_context(|| {
        format!(
            "publish: ensure binstall metadata for '{}' before cargo publish",
            crate_cfg.name
        )
    })?;
    log.verbose(&format!(
        "ensured [package.metadata.binstall] in {}/Cargo.toml before publish",
        crate_cfg.path
    ));
    Ok(())
}
