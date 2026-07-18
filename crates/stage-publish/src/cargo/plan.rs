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
    /// Per-crate intra-workspace publish dependencies (crate name → the
    /// co-published crates it depends on), the exact graph `order` was derived
    /// from. Reused by the publisher's post-publish `poll_crates_io_index`
    /// gate so the "does a later crate depend on this one?" decision reads the
    /// same edges that produced the order — they can never disagree.
    pub deps: HashMap<String, Vec<String>>,
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

    // Publish-order edges. A present `depends_on` (`Some`, including an
    // explicit empty list) is authoritative: config-load derivation either
    // populates EVERY crate's `depends_on` to `Some(..)`, or — on the failure
    // that motivates this fallback — leaves every crate `None`. So `None`
    // uniquely means "config-load derivation did not run"; in that case derive
    // the edges here from the crate's own `Cargo.toml`. Without this fallback,
    // that failure collapses every `depends_on` to empty and `topological_sort`
    // seeds an ALPHABETICAL order — which publishes e.g. `-stage-attest` before
    // the `-stage-checksum` it depends on and hard-fails on crates.io. The
    // manifest read here is the SAME source the post-publish index poll and the
    // wait-for-deps gate use, so the publish order can never disagree with the
    // dependency the publisher then blocks on.
    let member_names: HashSet<String> = all_crates.iter().map(|c| c.name.clone()).collect();
    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| cfgs.contains_key(&c.name))
        .map(|c| {
            let deps = match c.depends_on.as_ref() {
                Some(d) => d.clone(),
                None => {
                    let mut deps = anodizer_core::config::derive_depends_on_from_cargo_toml(
                        std::path::Path::new(&c.path),
                        &member_names,
                    );
                    // A crate never depends on itself. Real workspace crates
                    // have distinct paths so a manifest never lists its own
                    // package; this only guards a caller that points several
                    // crates at one manifest.
                    deps.retain(|d| d != &c.name);
                    deps
                }
            };
            (c.name.clone(), deps)
        })
        .collect();

    let order = topological_sort(&publishable);
    validate_publish_order(&order, &publishable)?;

    let deps: HashMap<String, Vec<String>> = publishable.iter().cloned().collect();

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
        deps,
    })
}

/// Fail loud if `order` would publish any crate before an intra-workspace
/// dependency it needs on the index.
///
/// [`topological_sort`] already yields a dependency-first order for an acyclic
/// `graph`, but a cycle (or any future regression that hands it an incomplete
/// graph) makes it append the remaining nodes in input order — silently
/// emitting an order that violates a real edge. crates.io would then reject the
/// dependent for an unresolvable version deep inside the publish loop, where the
/// retry layer misreports it as transient sparse-index propagation lag and burns
/// three futile retries before hard-failing. Validating the order here converts
/// that cryptic mid-publish failure into an instant, actionable pre-publish
/// error before any one-way-door publish runs.
fn validate_publish_order(order: &[String], graph: &[(String, Vec<String>)]) -> Result<()> {
    let pos: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    for (name, deps) in graph {
        let Some(&here) = pos.get(name.as_str()) else {
            continue;
        };
        for dep in deps {
            // Deps outside the publish set (already on the index, or skipped)
            // carry no ordering constraint — only co-published crates do.
            if let Some(&dep_pos) = pos.get(dep.as_str())
                && dep_pos >= here
            {
                anyhow::bail!(
                    "cargo publish order is broken: '{name}' depends on workspace crate \
                     '{dep}', but '{dep}' is scheduled at position {dep_pos} (at or after \
                     '{name}' at position {here}). Refusing to publish out of order — crates.io \
                     would reject '{name}' for an unresolvable '{dep}' dependency. This usually \
                     means a dependency cycle among the co-published crates.",
                );
            }
        }
    }
    Ok(())
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
