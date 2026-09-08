//! Planning pass of the archive stage: every output path the run will write,
//! resolved before a single byte is copied.
//!
//! The stage runs in two passes. This one is read-only: it walks every
//! crate × `archives:` entry × build target × format, renders the name
//! templates and records the output path each would produce, together with
//! the [`Claim`] the run-scoped [`ArchPathGuard`] checks. The driver runs the
//! guard over the whole plan and only then hands it to the execute pass in
//! `archive_config.rs`, so a name template that collides anywhere in the run
//! — across two binaries of one entry, two targets, or two crates — is
//! refused before the first output lands in `dist/`.
//!
//! [`ArchPathGuard`]: anodizer_core::arch_path_guard::ArchPathGuard

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anodizer_core::arch_path_guard::Claim;
use anodizer_core::artifact::{Artifact, matches_id_filter};
use anodizer_core::config::{ArchiveConfig, FormatOverride};
use anodizer_core::context::Context;
use anodizer_core::target::map_target;
use anyhow::{Context as _, Result, bail};

use crate::run_helpers::render_binary_outputs;
use crate::{
    default_binary_name_template, default_name_template, default_name_template_multi_crate,
};

/// Everything the stage will write for one crate.
pub(crate) struct CratePlan {
    pub crate_name: String,
    pub crate_dir: PathBuf,
    pub archive_cfgs: Vec<ArchiveConfig>,
    pub all_binaries: Vec<Artifact>,
    pub configs: Vec<ConfigPlan>,
}

/// One `archives:` entry of a crate.
pub(crate) struct ConfigPlan {
    /// Index into [`CratePlan::archive_cfgs`].
    pub index: usize,
    pub archive_id: String,
    /// The default-visible line explaining why the entry produces nothing
    /// (`if:` evaluated falsy); an entry with a note plans no targets.
    pub skip: Option<String>,
    pub is_meta: bool,
    pub name_tmpl: String,
    pub binary_name_tmpl: String,
    pub targets: Vec<TargetPlan>,
}

/// One build target of an entry.
pub(crate) struct TargetPlan {
    pub target: String,
    /// The `Binary` this target's naming context renders — the first
    /// selected binary, matching [`seed_target_context`]. Empty for a meta
    /// entry, which selects none.
    pub binary: Option<String>,
    pub selected_bins: Vec<Artifact>,
    pub group_variant: Option<String>,
    pub archive_stem: String,
    /// Every format of this target is `binary`, so no extra file is packed.
    pub binary_only: bool,
    pub formats: Vec<FormatPlan>,
}

/// One format of a target: the output it produces, or why it produces none.
pub(crate) struct FormatPlan {
    pub format: String,
    /// The default-visible line explaining why the format produces nothing
    /// (`none`, or `binary` on a meta entry).
    pub skip: Option<String>,
    pub archive_filename: String,
    pub archive_path: PathBuf,
    /// One per selected binary under `binary`; empty otherwise.
    pub binary_outputs: Vec<BinaryOutput>,
    /// The archive path was already on disk before this run wrote anything.
    pub replacing: bool,
    /// Template variables the naming context defined when the names were
    /// rendered, for the guard's remedy.
    pub exposed: BTreeSet<String>,
}

/// One raw binary written under `format: binary`.
pub(crate) struct BinaryOutput {
    pub stem: String,
    pub dest: PathBuf,
    /// The binary this output was named after, as `{{ .Binary }}` rendered
    /// it — the per-binary naming context each output is rendered under.
    pub binary: Option<String>,
    pub bin: Artifact,
    /// `dest` was already on disk before this run wrote anything.
    pub replacing: bool,
}

impl CratePlan {
    /// One claim per output path the plan will write, in write order.
    pub(crate) fn claims(&self) -> impl Iterator<Item = Claim<'_>> {
        self.configs.iter().flat_map(move |cfg| {
            cfg.targets.iter().flat_map(move |tp| {
                tp.formats
                    .iter()
                    .filter(|fp| fp.skip.is_none())
                    .flat_map(move |fp| {
                        let common = Claim {
                            path: &fp.archive_path,
                            stage: "archives",
                            artifact: "archive",
                            template_key: "name_template",
                            name_template: Some(&cfg.name_tmpl),
                            rendered: &fp.archive_filename,
                            crate_name: &self.crate_name,
                            target: Some(&tp.target),
                            amd64_variant: tp.group_variant.as_deref(),
                            entry: cfg.index,
                            binary: tp.binary.as_deref(),
                            exposed: &fp.exposed,
                        };
                        if fp.format == "binary" {
                            Box::new(fp.binary_outputs.iter().map(move |out| Claim {
                                path: &out.dest,
                                artifact: "binary",
                                name_template: Some(&cfg.binary_name_tmpl),
                                rendered: &out.stem,
                                binary: out.binary.as_deref(),
                                ..common
                            })) as Box<dyn Iterator<Item = Claim<'_>>>
                        } else {
                            Box::new(std::iter::once(common))
                        }
                    })
            })
        })
    }
}

/// Seed the per-target naming context: `Os` / `Arch` / `Target`, the
/// micro-architecture variant vars, `CrateName` and the group's `Binary`.
///
/// Shared with binstall/nix asset-name derivation so a derived `pkg_url`
/// cannot drift from the archive this stage writes. A v3-tuned group must
/// render the same `Amd64` suffix its binaries were named with, so the
/// group's variant overlays the target-derived baseline. `Binary` is
/// cleared for a meta entry: a stale value from an earlier target would
/// otherwise name a meta archive after a binary it does not carry.
pub(crate) fn seed_target_context(
    ctx: &mut Context,
    target: &str,
    crate_name: &str,
    selected_bins: &[Artifact],
    group_variant: Option<&str>,
) {
    anodizer_core::archive_name::seed_target_vars(ctx, target);
    let (_, group_arch) = map_target(target);
    anodizer_core::archive_name::seed_amd64_variant_var(
        ctx.template_vars_mut(),
        &group_arch,
        group_variant,
    );
    let tvars = ctx.template_vars_mut();
    tvars.set("CrateName", crate_name);
    tvars.set(
        "Binary",
        &selected_bins
            .first()
            .map(|b| b.binary_name().unwrap_or_default())
            .unwrap_or_default(),
    );
}

/// The inputs that are constant for one archive-stage run, resolved once in
/// `run()` and shared by every crate's plan.
pub(crate) struct RunInputs<'a> {
    pub dist: &'a Path,
    /// The run archives more than one crate, so names must carry the crate.
    pub multi_crate: bool,
    pub default_format: &'a str,
    pub format_overrides: &'a [FormatOverride],
}

/// Resolve every output the stage will write for one crate.
pub(crate) fn plan_crate(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    run: &RunInputs<'_>,
    crate_name: &str,
    crate_dir: &Path,
    archive_cfgs: &[ArchiveConfig],
    all_binaries: &[Artifact],
) -> Result<CratePlan> {
    let RunInputs {
        dist,
        multi_crate,
        default_format: global_default_format,
        format_overrides: global_format_overrides,
    } = *run;
    let mut configs = Vec::with_capacity(archive_cfgs.len());
    for (index, archive_cfg) in archive_cfgs.iter().enumerate() {
        let archive_id = archive_cfg.id.as_deref().unwrap_or("default").to_string();
        let proceed = anodizer_core::config::evaluate_if_condition(
            archive_cfg.if_condition.as_deref(),
            &format!("archive config '{archive_id}'"),
            |t| ctx.render_template(t),
        )?;
        let is_meta = archive_cfg.meta.unwrap_or(false);
        let name_tmpl = archive_cfg.name_template.clone().unwrap_or_else(|| {
            if multi_crate {
                default_name_template_multi_crate()
            } else {
                default_name_template()
            }
            .to_string()
        });
        // `binary` format names each output after the BINARY it holds rather
        // than the project, so its default template differs; an explicit
        // `name_template` still wins.
        let binary_name_tmpl = if archive_cfg.name_template.is_some() {
            name_tmpl.clone()
        } else {
            default_binary_name_template().to_string()
        };
        let mut plan = ConfigPlan {
            index,
            archive_id: archive_id.clone(),
            skip: None,
            is_meta,
            name_tmpl,
            binary_name_tmpl,
            targets: Vec::new(),
        };
        if !proceed {
            plan.skip = Some(format!(
                "skipped archives[{archive_id}] — `if` condition evaluated falsy"
            ));
            configs.push(plan);
            continue;
        }

        let Some(by_target) =
            group_binaries_by_target(ctx, log, archive_cfg, crate_name, all_binaries)?
        else {
            continue;
        };

        // Completion/man generation binds `Shell` and `ArtifactPath` and
        // leaves them empty; the names planned here must render in that
        // same scope, or a template naming either would claim one path and
        // write another.
        if archive_cfg.completions.is_some() || archive_cfg.manpages.is_some() {
            crate::completions_gen::clear_generate_vars(ctx);
        }

        let format_overrides: Vec<FormatOverride> = archive_cfg
            .format_overrides
            .clone()
            .unwrap_or_else(|| global_format_overrides.to_vec());

        for (target, target_bins) in &by_target {
            let selected_bins: Vec<Artifact> = target_bins
                .iter()
                .filter(|b| match &archive_cfg.binaries {
                    None => true,
                    // Same name a `binaries:` entry would be written against:
                    // matching a missing key as "" drops the binary silently
                    // and skips the whole target.
                    Some(names) => names.contains(&b.binary_name().unwrap_or_default()),
                })
                .cloned()
                .collect();
            if selected_bins.is_empty() && !is_meta {
                continue;
            }

            let formats_to_produce = formats_for_target(
                target,
                &format_overrides,
                archive_cfg,
                global_default_format,
            );
            let group_variant: Option<String> = selected_bins
                .first()
                .and_then(|b| b.metadata.get("amd64_variant"))
                .cloned();
            seed_target_context(
                ctx,
                target,
                crate_name,
                &selected_bins,
                group_variant.as_deref(),
            );

            let archive_stem = ctx
                .render_template(&plan.name_tmpl)
                .with_context(|| format!("render archive name for {crate_name}/{target}"))?;
            // An empty stem produces a hidden file like `dist/.tar.gz` that
            // downstream stages cannot resolve by canonical name.
            if archive_stem.is_empty() {
                bail!(
                    "archive: rendered archive name template '{}' \
                 produced an empty stem for crate '{}' target \
                 '{}'. An empty stem yields a hidden output path \
                 (`dist/.<format>`) that the duplicate-name \
                 detector and downstream stages cannot resolve. \
                 Verify the template references variables that \
                 are populated on this run (e.g. `{{{{ Tag }}}}` is \
                 unset during `--snapshot` — use \
                 `{{{{ Version }}}}` or the default \
                 `archive.name_template` instead).",
                    plan.name_tmpl,
                    crate_name,
                    target
                );
            }

            let mut formats = Vec::with_capacity(formats_to_produce.len());
            for format in formats_to_produce {
                let skip = if format == "none" {
                    Some(format!(
                        "skipped archive for {crate_name}/{target} — format: none"
                    ))
                } else if format == "binary" && selected_bins.is_empty() {
                    // A meta entry carries no binaries, so `binary` has
                    // nothing to emit for it.
                    Some(format!(
                        "skipped archive for {crate_name}/{target} — meta archive \
                         under format: binary carries no binaries"
                    ))
                } else {
                    None
                };
                let bin_refs: Vec<&Artifact> = selected_bins.iter().collect();
                let binary_outputs: Vec<BinaryOutput> = if skip.is_none() && format == "binary" {
                    render_binary_outputs(
                        ctx,
                        &bin_refs,
                        &plan.binary_name_tmpl,
                        dist,
                        crate_name,
                        target,
                    )?
                    .into_iter()
                    .map(|(stem, dest, bin)| BinaryOutput {
                        replacing: dest.exists(),
                        stem,
                        dest,
                        binary: bin.binary_name(),
                        bin: bin.clone(),
                    })
                    .collect()
                } else {
                    Vec::new()
                };
                let archive_filename = if format == "binary" {
                    binary_outputs
                        .first()
                        .map(|out| out.stem.clone())
                        .unwrap_or_default()
                } else {
                    format!("{archive_stem}.{format}")
                };
                let archive_path = dist.join(&archive_filename);
                formats.push(FormatPlan {
                    replacing: format != "binary" && archive_path.exists(),
                    exposed: ctx.template_vars().defined_names(),
                    format,
                    skip,
                    archive_filename,
                    archive_path,
                    binary_outputs,
                });
            }

            plan.targets.push(TargetPlan {
                target: target.clone(),
                binary_only: formats.iter().all(|f| f.format == "binary"),
                binary: selected_bins.first().and_then(|b| b.binary_name()),
                selected_bins,
                group_variant,
                archive_stem,
                formats,
            });
        }
        configs.push(plan);
    }

    Ok(CratePlan {
        crate_name: crate_name.to_string(),
        crate_dir: crate_dir.to_path_buf(),
        archive_cfgs: archive_cfgs.to_vec(),
        all_binaries: all_binaries.to_vec(),
        configs,
    })
}

/// The entry's binaries (after its `ids:` filter) grouped by build target,
/// or `None` when the entry has nothing to archive.
///
/// `BTreeMap` is load-bearing: the map is iterated to register one archive
/// per target, and `HashMap` order is randomised per process, which would
/// surface as per-run drift in `dist/artifacts.json`.
fn group_binaries_by_target(
    ctx: &Context,
    log: &anodizer_core::log::StageLogger,
    archive_cfg: &ArchiveConfig,
    crate_name: &str,
    all_binaries: &[Artifact],
) -> Result<Option<BTreeMap<String, Vec<Artifact>>>> {
    let archive_id = archive_cfg.id.as_deref().unwrap_or("default");
    let is_meta = archive_cfg.meta.unwrap_or(false);
    let binaries: Vec<Artifact> = if is_meta {
        Vec::new()
    } else if archive_cfg.ids.is_some() {
        all_binaries
            .iter()
            .filter(|a| matches_id_filter(a, archive_cfg.ids.as_deref()))
            .cloned()
            .collect()
    } else {
        all_binaries.to_vec()
    };

    if binaries.is_empty() && !is_meta {
        let id_filter_desc = archive_cfg
            .ids
            .as_deref()
            .map(|ids| format!(" matching ids {ids:?}"))
            .unwrap_or_default();
        // A mis-scoped `ids:` filter (or a crate with no binaries) produces
        // no archive at all; GoReleaser errors here. Hard-fail under
        // `--strict` so the config doesn't silently yield nothing, and warn
        // otherwise.
        ctx.strict_guard(
            log,
            &format!(
                "skipped archives[{archive_id}] — crate {crate_name} has no \
             binaries{id_filter_desc} (set `meta: true` if this is intentional)"
            ),
        )?;
        return Ok(None);
    }

    let mut by_target: BTreeMap<String, Vec<Artifact>> = BTreeMap::new();
    for bin in binaries {
        let target = bin.target.clone().unwrap_or_else(|| "unknown".to_string());
        by_target.entry(target).or_default().push(bin);
    }
    if is_meta && by_target.is_empty() {
        by_target.insert("unknown".to_string(), Vec::new());
    }

    // The "binary" format is exempt from the equal-count check, as in
    // GoReleaser.
    let is_binary_format = archive_cfg
        .formats
        .as_ref()
        .map(|fs| fs.iter().any(|f| f == "binary"))
        .unwrap_or(false);
    if !is_binary_format
        && !archive_cfg.allow_different_binary_count.unwrap_or(false)
        && by_target.len() > 1
    {
        let counts: Vec<usize> = by_target.values().map(|bins| bins.len()).collect();
        let first = counts[0];
        if counts.iter().any(|&c| c != first) {
            let details: Vec<_> = by_target
                .iter()
                .map(|(t, b)| format!("{t}={}", b.len()))
                .collect();
            bail!(
                "binary counts differ across targets ({:?}); set allow_different_binary_count: true to allow this",
                details
            );
        }
    }
    Ok(Some(by_target))
}

/// The formats one target produces: a `format_overrides[]` entry matching
/// its OS wins, else the entry's `formats`, else the global default.
///
/// The OS match is by prefix so an OS sub-variant (`linux-musl`) picks up
/// its base OS's override without a config change; an empty `os:` is
/// rejected as a typo guard, since it would match every target.
fn formats_for_target(
    target: &str,
    format_overrides: &[FormatOverride],
    archive_cfg: &ArchiveConfig,
    global_default_format: &str,
) -> Vec<String> {
    let (os, _arch) = map_target(target);
    let override_match = format_overrides
        .iter()
        .find(|ov| !ov.os.is_empty() && os.starts_with(&ov.os))
        .and_then(|ov| ov.formats.as_ref().filter(|f| !f.is_empty()).cloned());
    match override_match {
        Some(fmts) => fmts,
        None => archive_cfg
            .formats
            .as_ref()
            .filter(|f| !f.is_empty())
            .cloned()
            .unwrap_or_else(|| vec![global_default_format.to_string()]),
    }
}
