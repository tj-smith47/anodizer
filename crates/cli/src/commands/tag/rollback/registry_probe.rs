use super::guard::{BurnProbes, MAX_PROBE_WORKERS};
use super::types::{RollbackRefusal, refusal_next_step};
use anodizer_core::git;
use anodizer_core::log::StageLogger;
use anyhow::{Result, bail};

/// How a tag maps onto the config's crate universe for the crates.io burn
/// probe. The split lets the guard distinguish "nothing to probe because
/// none of the tag's crates target crates.io" (safe to proceed) from
/// "the tag maps to no crate at all" (the probe is blind — fail closed
/// when the config publishes to crates.io elsewhere).
pub(super) struct TagCrateMapping {
    /// `(crate name, version)` pairs the tag stamps on crates.io.
    pub(super) probes: Vec<(String, String)>,
    /// Crates whose tag family matched but which don't publish to
    /// crates.io (no `publish.cargo` block, or a custom `registry:`/
    /// `index:` target outside the probe's scope).
    pub(super) matched_non_crates_io: usize,
}

/// Resolve the `(crate name, version)` pairs a tag stamps on crates.io, per
/// the repo config: every crate whose `publish.cargo` block targets
/// crates.io (per the publisher's own [`targets_crates_io`] judgment —
/// custom `registry:`/`index:` targets are out of the probe's scope) and
/// whose tag family prefix (from its `tag_template`, monorepo prefix
/// stripped) matches the tag. Per-crate tags (`crd-v0.5.0`) resolve to
/// their own crate — note the tag prefix is the template's, NOT the crate
/// name (cfgd's `crd-v...` family belongs to the crate `cfgd-crd`);
/// lockstep tags (every crate sharing the bare `v...` family) resolve to
/// every such crate.
///
/// Publish-time `skip:`/`if:` gating is deliberately NOT evaluated (no
/// template context exists in a rollback): a gated crate may be probed even
/// though the release never publishes it, which can only tighten the guard
/// (`--force` remains the escape hatch), never loosen it.
///
/// [`targets_crates_io`]: anodizer_stage_publish::cargo::targets_crates_io
pub(super) fn crates_io_versions_for_tag(
    config: &anodizer_core::config::Config,
    tag: &str,
) -> TagCrateMapping {
    let mut mapping = TagCrateMapping {
        probes: Vec::new(),
        matched_non_crates_io: 0,
    };
    for (c, version) in crates_versioned_by_tag(config, tag) {
        match c.publish.as_ref().and_then(|p| p.cargo.as_ref()) {
            Some(cargo_cfg)
                if anodizer_stage_publish::cargo::targets_crates_io(Some(cargo_cfg)) =>
            {
                mapping.probes.push((c.name.clone(), version));
            }
            _ => mapping.matched_non_crates_io += 1,
        }
    }
    mapping
}

/// Resolve which crates a tag versions, per the repo config's crate tag
/// families: every crate whose tag family prefix (from its `tag_template`,
/// monorepo prefix stripped) matches the tag, paired with the semver the tag
/// stamps on it. Shared by every registry-specific burn probe so the tag →
/// crate judgment exists exactly once. Works across all three layouts:
/// single-crate and lockstep tags (`v0.5.0`) match every crate sharing the
/// bare family; per-crate tags (`crd-v0.5.0`) match their own crate only.
pub(super) fn crates_versioned_by_tag<'c>(
    config: &'c anodizer_core::config::Config,
    tag: &str,
) -> Vec<(&'c anodizer_core::config::CrateConfig, String)> {
    let stripped = match config.monorepo_tag_prefix() {
        Some(prefix) => git::strip_monorepo_prefix(tag, prefix),
        None => tag,
    };
    let mut out = Vec::new();
    for c in config.crate_universe() {
        let prefix = git::per_crate_tag_prefix(&c.name, c.tag_template.as_deref().unwrap_or(""));
        let Some(version) = stripped.strip_prefix(&prefix) else {
            continue;
        };
        if git::parse_semver(version).is_err() {
            continue;
        }
        out.push((c, version.to_string()));
    }
    out
}

/// Layer 2 of [`check_not_irreversibly_published`]: refuse rollback when
/// any tag's crates.io-targeting crate@version is live on the crates.io
/// sparse index — GLOBAL registry state, consulted regardless of what this
/// run's summaries say (a prior run may have burned the version; its
/// summary lives on another runner's disk).
///
/// - version on the index → REFUSE (burned; fix forward).
/// - index unreachable → REFUSE (fail closed: publication state is
///   unverifiable, and gambling a destructive delete on a transient outage
///   is the poison-guard anti-pattern). `--force` is the operator escape.
/// - tag maps to NO crate while the config publishes to crates.io →
///   REFUSE (fail closed: the tag→crate mapping is the probe's eyes; a
///   tag it cannot map might version a crate that IS burned).
/// - tag maps only to crates that don't target crates.io, or the config
///   publishes nothing to crates.io at all → proceed: there is no cargo
///   one-way door for this config to have burned.
///
/// Repeated `crate@version` probes are deduplicated (the same pair recurs
/// under `Scope::All` when tag families overlap, e.g. a monorepo-prefixed
/// and a bare tag resolving to the same crate).
pub(super) fn check_not_burned_on_crates_io(
    tags: &[String],
    unsummarized: &[String],
    config: &anodizer_core::config::Config,
    index_probe: &(dyn Fn(&str, &str) -> Result<bool> + Sync),
    log: &StageLogger,
) -> Result<()> {
    let config_targets_crates_io = config.crate_universe().iter().any(|c| {
        c.publish
            .as_ref()
            .and_then(|p| p.cargo.as_ref())
            .is_some_and(|cfg| anodizer_stage_publish::cargo::targets_crates_io(Some(cfg)))
    });
    if !config_targets_crates_io {
        log.status(
            "no crate in the config publishes to crates.io — no cargo one-way door to probe",
        );
        return Ok(());
    }
    let mut burned: Vec<String> = Vec::new();
    let mut squat_suspect_crates: Vec<String> = Vec::new();
    let mut indeterminate: Vec<String> = Vec::new();
    let mut unmapped: Vec<String> = Vec::new();
    let mut probed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // Pass 1 — resolve every tag's deduplicated `(tag, crate, version)`
    // probe set up front so the network round-trips can run concurrently.
    let mut pending: Vec<(String, String, String)> = Vec::new();
    for tag in tags {
        let mapping = crates_io_versions_for_tag(config, tag);
        if mapping.probes.is_empty() {
            if mapping.matched_non_crates_io > 0 {
                log.status(&format!(
                    "no crates.io-targeting crate is versioned by {tag} — no cargo \
                     one-way door to probe"
                ));
            } else {
                unmapped.push(format!("  {tag}"));
            }
            continue;
        }
        for (name, version) in mapping.probes {
            if !probed.insert((name.clone(), version.clone())) {
                continue;
            }
            pending.push((tag.clone(), name, version));
        }
    }
    // Pass 2 — probe the index concurrently: a 20-crate lockstep workspace
    // must not serialize 20 network ladders during a registry outage. Each
    // probe's own Result is wrapped so a failed probe (fail-closed evidence,
    // classified below) doesn't abort its in-flight siblings; a worker panic
    // surfaces as an attributed error.
    let results = anodizer_core::parallel::run_parallel_chunks(
        &pending,
        MAX_PROBE_WORKERS,
        "crates.io burn probe",
        log,
        |(_, name, version)| Ok(index_probe(name, version)),
    )?;
    // Pass 3 — classify in the deterministic pass-1 order, so the operator
    // output is stable regardless of probe completion order.
    for ((tag, name, version), result) in pending.iter().zip(results) {
        match result {
            Ok(true) => {
                if unsummarized.contains(tag) && !squat_suspect_crates.contains(name) {
                    squat_suspect_crates.push(name.clone());
                }
                burned.push(format!("  {tag}: {name}@{version}"));
            }
            Ok(false) => log.status(&format!(
                "'{name}@{version}' is not on the crates.io index — {tag} carries no \
                 cargo one-way door"
            )),
            Err(e) => indeterminate.push(format!("  {tag}: {name}@{version} ({e:#})")),
        }
    }
    if !burned.is_empty() {
        // A local run summary is per-runner and ephemeral: a fresh CI runner
        // holds no summary for a burn a prior runner landed, so its absence
        // is expected for a legitimate own-publish and is NOT evidence of
        // foreign ownership. The note leads with that likely case and offers
        // the crates.io page only so the rare squatting possibility can be
        // ruled out — it never implies the version isn't the operator's own.
        let squat_note = if squat_suspect_crates.is_empty() {
            String::new()
        } else {
            let urls = squat_suspect_crates
                .iter()
                .map(|name| format!("https://crates.io/crates/{name}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "\nNo local run summary corroborates this publish — most likely a prior \
                 run of yours (on CI, summaries live on each runner's disk and don't \
                 carry over); far less likely, the name is held by someone else. Confirm \
                 ownership at {urls} before assuming either."
            )
        };
        return Err(RollbackRefusal {
            reason: format!(
                "these version(s) are live on the crates.io index (published by a prior \
                 attempt, whatever this run's summaries say):\n{}\n\
                 crates.io never accepts the same version twice, so deleting the tag(s) \
                 cannot lead to a clean same-version re-cut — tags kept to protect the \
                 published state.{squat_note}",
                burned.join("\n")
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    if !indeterminate.is_empty() {
        bail!(
            "refusing to roll back: the crates.io index could not be reached to verify \
             whether these version(s) are already published:\n{}\n\
             Without the index there is no proof the version(s) are safe to destroy — a \
             prior run may have burned them on crates.io. Restore network access and \
             retry, or pass --force if you are certain nothing irreversible shipped.",
            indeterminate.join("\n")
        );
    }
    if !unmapped.is_empty() {
        bail!(
            "refusing to roll back — could not map these tag(s) to any crate in the \
             anodizer config:\n{}\n\
             The crates.io burn probe works by mapping each tag's family (from the crates' \
             tag_template) to the crates it versions, and this config publishes crate(s) to \
             crates.io — a tag the probe cannot map might version a crate whose version is \
             already burned there, so proceeding blind is not safe. Check that the config's \
             crates/tag_template families cover these tag(s), or pass --force if you are \
             certain nothing irreversible shipped.",
            unmapped.join("\n")
        );
    }
    Ok(())
}

/// One pending immutable-registry probe (npm or pypi), collected up front so
/// the network round-trips run on the shared bounded pool.
pub(super) enum ImmutableProbe {
    Npm {
        tag: String,
        registry: String,
        package: String,
        version: String,
    },
    Pypi {
        tag: String,
        repository: String,
        project: String,
        version: String,
    },
}

/// Layer 2 (alongside [`check_not_burned_on_crates_io`]): refuse rollback when
/// any tag's configured npm package or PyPI project@version is already live on
/// its registry — both are true immutable one-way doors (a published npm
/// version can never be cleanly re-cut; a PyPI filename is a permanent index
/// slot), so the same GLOBAL-state, fail-closed treatment crates.io gets
/// applies: a prior run on another runner may have burned the version, leaving
/// no local summary. Every tag is probed live — matching the crates.io
/// sibling. A summarized tag is NOT skipped: layer 1 only refuses when the
/// summary *records* the Submitter publish as landed, so it misses the
/// immutable-door verification race (the version landed at the registry but was
/// recorded as failed — a read timeout after the 201). The live probe is
/// exactly what closes that race, so it must run for summarized tags too.
///
/// - version live on the registry → REFUSE (burned; fix forward).
/// - registry unreachable → REFUSE (fail closed: publication state
///   unverifiable — gambling a destructive delete on a transient outage is the
///   poison-guard anti-pattern). `--force` is the operator escape.
/// - package name / endpoint templated (unresolvable without a release
///   context) while the config publishes to npm/pypi → REFUSE (fail closed:
///   the guard cannot prove the immutable version it would orphan is safe).
///   This is the npm/pypi analogue of crates.io's unmappable-tag posture, and
///   is STRICTER than the moderated-registry probe's warn-and-skip — those
///   registries are additionally covered by run-summary evidence and are only
///   advisory, whereas an unresolvable immutable door leaves a real burn
///   possibility unproven.
/// - a config that publishes nothing to npm/pypi, or whose entries aren't
///   versioned by the tag → proceed: no such one-way door exists to have
///   burned.
pub(super) fn check_not_burned_on_npm_pypi(
    tags: &[String],
    config: &anodizer_core::config::Config,
    probes: &BurnProbes<'_>,
    log: &StageLogger,
) -> Result<()> {
    let npm_entries = config.npms.as_deref().unwrap_or(&[]);
    let pypi_entries = config.pypis.as_deref().unwrap_or(&[]);
    if npm_entries.is_empty() && pypi_entries.is_empty() {
        log.status("no npm/pypi publisher is configured — no immutable one-way door to probe");
        return Ok(());
    }
    // Pass 1 — resolve every deduplicated probe up front so the network
    // round-trips run concurrently. Every tag is probed (like crates.io): the
    // live index is the only evidence that catches an immutable-door landing
    // the run summary recorded as a failure.
    let mut pending: Vec<ImmutableProbe> = Vec::new();
    let mut unresolvable: Vec<String> = Vec::new();
    let mut probed: std::collections::HashSet<(&'static str, String, String)> =
        std::collections::HashSet::new();
    for tag in tags {
        let versioned: std::collections::HashMap<&str, String> =
            crates_versioned_by_tag(config, tag)
                .into_iter()
                .map(|(c, v)| (c.name.as_str(), v))
                .collect();
        for cfg in npm_entries {
            let crate_name = anodizer_stage_publish::npm::static_entry_crate_name(config);
            let Some(version) = versioned.get(crate_name.as_str()) else {
                continue;
            };
            match (
                anodizer_stage_publish::npm::static_registry(cfg),
                anodizer_stage_publish::npm::static_published_name(&crate_name, cfg),
            ) {
                (Some(registry), Some(package)) => {
                    if probed.insert(("npm", format!("{registry}#{package}"), version.clone())) {
                        pending.push(ImmutableProbe::Npm {
                            tag: tag.clone(),
                            registry,
                            package,
                            version: version.clone(),
                        });
                    }
                }
                _ => unresolvable.push(format!(
                    "  {tag}: npm package for crate '{crate_name}' — its name or registry is a \
                     template expression that cannot be resolved outside a release run"
                )),
            }
        }
        for cfg in pypi_entries {
            let crate_name = anodizer_stage_publish::pypi::static_entry_crate_name(config, cfg);
            let Some(version) = versioned.get(crate_name.as_str()) else {
                continue;
            };
            match (
                anodizer_stage_publish::pypi::static_repository(cfg),
                anodizer_stage_publish::pypi::static_project_name(&crate_name, cfg),
            ) {
                (Some(repository), Some(project)) => {
                    if probed.insert(("pypi", format!("{repository}#{project}"), version.clone())) {
                        pending.push(ImmutableProbe::Pypi {
                            tag: tag.clone(),
                            repository,
                            project,
                            version: version.clone(),
                        });
                    }
                }
                _ => unresolvable.push(format!(
                    "  {tag}: pypi project for crate '{crate_name}' — its name or repository is a \
                     template expression that cannot be resolved outside a release run"
                )),
            }
        }
    }
    // Pass 2 — probe the registries concurrently; each probe's own Result is
    // wrapped so a failed probe (fail-closed evidence, classified below)
    // doesn't abort its in-flight siblings, and a worker panic surfaces as an
    // attributed error.
    let results = anodizer_core::parallel::run_parallel_chunks(
        &pending,
        MAX_PROBE_WORKERS,
        "npm/pypi burn probe",
        log,
        |probe| {
            Ok(match probe {
                ImmutableProbe::Npm {
                    registry,
                    package,
                    version,
                    ..
                } => (probes.npm)(registry, package, version),
                ImmutableProbe::Pypi {
                    repository,
                    project,
                    version,
                    ..
                } => (probes.pypi)(repository, project, version),
            })
        },
    )?;
    // Pass 3 — classify in the deterministic pass-1 order.
    let mut burned: Vec<String> = Vec::new();
    let mut indeterminate: Vec<String> = Vec::new();
    for (probe, result) in pending.iter().zip(results) {
        match probe {
            ImmutableProbe::Npm {
                tag,
                registry,
                package,
                version,
            } => match result {
                Ok(true) => {
                    burned.push(format!("  {tag}: npm '{package}@{version}' on {registry}"))
                }
                Ok(false) => log.status(&format!(
                    "npm '{package}@{version}' is not published on {registry} — {tag} carries \
                     no npm one-way door"
                )),
                Err(e) => indeterminate.push(format!(
                    "  {tag}: npm '{package}@{version}' on {registry} ({e:#})"
                )),
            },
            ImmutableProbe::Pypi {
                tag,
                repository,
                project,
                version,
            } => match result {
                Ok(true) => burned.push(format!(
                    "  {tag}: pypi '{project}=={version}' on {repository}"
                )),
                Ok(false) => log.status(&format!(
                    "pypi '{project}=={version}' is not released on {repository} — {tag} \
                     carries no pypi one-way door"
                )),
                Err(e) => indeterminate.push(format!(
                    "  {tag}: pypi '{project}=={version}' on {repository} ({e:#})"
                )),
            },
        }
    }
    if !burned.is_empty() {
        return Err(RollbackRefusal {
            reason: format!(
                "these version(s) are already live on an immutable registry (published by a \
                 prior attempt, whatever this run's summaries say):\n{}\n\
                 npm never accepts the same version cleanly and a PyPI filename can never be \
                 re-uploaded, so deleting the tag(s) cannot lead to a clean same-version \
                 re-cut — tags kept to protect the published state.",
                burned.join("\n")
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    if !indeterminate.is_empty() {
        bail!(
            "refusing to roll back: an immutable registry could not be reached to verify \
             whether these version(s) are already published:\n{}\n\
             Without the registry there is no proof the version(s) are safe to destroy — a \
             prior run may have burned them on npm/pypi. Restore network access and retry, \
             or pass --force if you are certain nothing irreversible shipped.",
            indeterminate.join("\n")
        );
    }
    if !unresolvable.is_empty() {
        bail!(
            "refusing to roll back — could not resolve the npm/pypi package name(s) for \
             these tag(s) without a release template context:\n{}\n\
             This config publishes to npm/pypi, and an unresolvable package name might \
             version a package that IS burned there, so proceeding blind is not safe. \
             Roll back from a release checkout, or pass --force if you are certain nothing \
             irreversible shipped.",
            unresolvable.join("\n")
        );
    }
    Ok(())
}

/// Collect every parseable run summary under `<dist>/run-*/summary.json`
/// (single-crate / lockstep layout) and `<dist>/<crate>/run-*/summary.json`
/// (per-crate workspace layout). Unreadable or unparseable files warn
/// and are skipped — they carry no usable evidence either way.
pub(super) fn collect_run_summaries(
    dist: &std::path::Path,
    log: &StageLogger,
) -> Vec<anodizer_stage_publish::run_summary::RunSummary> {
    let mut out = Vec::new();
    for path in anodizer_stage_publish::run_summary::collect_run_summary_paths(dist) {
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str(&text)?))
        {
            Ok(summary) => out.push(summary),
            Err(e) => log.warn(&format!(
                "ignoring unreadable run summary {}: {e:#}",
                path.display()
            )),
        }
    }
    out
}
