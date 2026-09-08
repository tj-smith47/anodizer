//! Per-crate archive-config processing, split from `run.rs`.
//!
//! Holds [`archive_one_config`], the heaviest helper the
//! [`crate::ArchiveStage`] driver calls into: it processes every `archives:`
//! block attached to one crate.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::{ArchiveConfig, ArchiveFileSpec, FormatOverride};
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::target::map_target;
use anyhow::{Context as _, Result, bail};

use crate::entries::{ArchiveEntry, deduplicate_entries, sort_entries};
use crate::file_specs::{
    ResolvedExtraFile, render_file_info, resolve_default_extra_files, resolve_file_specs,
};
use crate::formats;
use crate::run::resolve_host_binary;
use crate::run_helpers::{
    claim_output_path, render_archive_templated_files, render_binary_outputs,
    resolve_archive_mtime, write_archive_in_format,
};
use crate::{
    default_binary_name_template, default_name_template, default_name_template_multi_crate,
};

/// Process every `archives:` config attached to a single crate.
/// Iterates the configs, applies per-config filters and format
/// overrides, and appends one `Archive` (or per-binary
/// `UploadableBinary`) artifact per (target, format) combination to
/// `new_artifacts`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn archive_one_config(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    dist: &Path,
    dry_run: bool,
    multi_crate: bool,
    global_default_format: &str,
    global_format_overrides: &[FormatOverride],
    archive_cfgs: &[ArchiveConfig],
    crate_name: &str,
    crate_dir: &Path,
    all_binaries: &[Artifact],
    new_artifacts: &mut Vec<Artifact>,
    name_guard: &mut anodizer_core::arch_path_guard::ArchPathGuard,
) -> Result<()> {
    for archive_cfg in archive_cfgs {
        // The archive id labels every diagnostic + staging path for this
        // config; bound once here and reused across the per-target / per-format
        // inner loops below.
        let archive_id = archive_cfg.id.as_deref().unwrap_or("default");
        // `archives[].if:` conditional gate. Skip the entire archive
        // config (no archives produced for this id) when the rendered
        // condition is falsy.
        let proceed = anodizer_core::config::evaluate_if_condition(
            archive_cfg.if_condition.as_deref(),
            &format!("archive config '{archive_id}'"),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status(&format!(
                "skipped archives[{archive_id}] — `if` condition evaluated falsy"
            ));
            continue;
        }

        let is_meta = archive_cfg.meta.unwrap_or(false);

        // ids filtering: only include binary artifacts whose metadata
        // "id" matches one of the listed IDs (same pattern as checksum)
        let binaries: Vec<Artifact> = if is_meta {
            // Meta archives have no binaries
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
            continue;
        }

        // Group binaries by target.
        //
        // `BTreeMap` (not `HashMap`) is load-bearing: this map is
        // iterated below to register one archive Artifact per target,
        // and `HashMap` iteration order is randomised per process via
        // `RandomState`. Two runs at identical inputs would therefore
        // register `linux-amd64` before `linux-arm64` in run #1 and
        // the reverse in run #2 — same set, different order. That
        // order then bakes into `dist/artifacts.json` as an
        // observable per-run drift (archive entries appearing in
        // swapped positions across runs). `BTreeMap` orders by key
        // (the target triple), so iteration is identical across
        // runs regardless of insertion order.
        let mut by_target: BTreeMap<String, Vec<Artifact>> = BTreeMap::new();
        for bin in &binaries {
            let target = bin.target.clone().unwrap_or_else(|| "unknown".to_string());
            by_target.entry(target).or_default().push(bin.clone());
        }

        // For meta archives with no binaries, create a single entry with "unknown" target
        if is_meta && by_target.is_empty() {
            by_target.insert("unknown".to_string(), Vec::new());
        }

        // allow_different_binary_count check: when false (default),
        // error if different targets have different binary counts
        // (errors, not warns).
        // The "binary" format is exempted
        // format from this check via `slices.Contains(archive.Formats,
        // "binary")`.
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

        // Determine format overrides (per-config > global)
        let format_overrides: Vec<FormatOverride> = archive_cfg
            .format_overrides
            .clone()
            .unwrap_or_else(|| global_format_overrides.to_vec());

        // Determine which binaries to include
        let binary_filter: Option<&Vec<String>> = archive_cfg.binaries.as_ref();

        // Name template — pick the multi-crate default when the run
        // produces archives for more than one crate so per-crate
        // filenames do not collide. User-set `archive.name_template:`
        // always wins regardless of multi-crate state.
        let has_custom_name_tmpl = archive_cfg.name_template.is_some();
        let default_tmpl = if multi_crate {
            default_name_template_multi_crate()
        } else {
            default_name_template()
        };
        let name_tmpl = archive_cfg.name_template.as_deref().unwrap_or(default_tmpl);
        // `binary` format names each output after the BINARY it holds rather
        // than the project, so its default template differs; an explicit
        // `name_template` still wins.
        let binary_name_tmpl = if has_custom_name_tmpl {
            name_tmpl
        } else {
            default_binary_name_template()
        };

        // strip_binary_directory: place binaries at archive root
        let strip_bin_dir = archive_cfg.strip_binary_directory.unwrap_or(false);

        // Generate (or harvest/copy) completion + man files ONCE for this
        // archive config and stage them in dist so the SAME files feed both
        // the archive and nfpm `contents:` globs. Mode A reuses the
        // host-native binary's output for every target (arch-independent).
        let aux_files: Vec<ResolvedExtraFile> =
            if archive_cfg.completions.is_some() || archive_cfg.manpages.is_some() {
                let host_binary = resolve_host_binary(all_binaries);
                crate::completions_gen::generate_archive_aux_files(
                    ctx,
                    archive_cfg.completions.as_ref(),
                    archive_cfg.manpages.as_ref(),
                    crate_name,
                    crate_dir,
                    host_binary,
                    dist,
                    dry_run,
                    log,
                )?
            } else {
                Vec::new()
            };

        // Hook firing happens INSIDE the format loop below so each
        // (target, format) pair gets its own before/after pair with
        // `.Format` / `.Os` / `.Arch` / `.Target` / `.ArtifactPath` /
        // `.ArtifactName` / `.ArtifactExt` / `.ArtifactID` set on the
        // live template-var scope. Contract: "If
        // multiple formats are set, hooks will be executed for each
        // format" and "Extra template fields
        // available: `.Format`".
        let pre_label = format!("pre-archive[{archive_id}]");
        let post_label = format!("post-archive[{archive_id}]");

        for (target, target_bins) in &by_target {
            // Filter binaries for this archive config
            let selected_bins: Vec<&Artifact> = target_bins
                .iter()
                .filter(|b| match binary_filter {
                    None => true,
                    Some(names) => {
                        // Same name a `binaries:` entry would be written
                        // against: matching a missing key as "" drops the
                        // binary silently and skips the whole target.
                        names.contains(&b.binary_name().unwrap_or_default())
                    }
                })
                .collect();

            if selected_bins.is_empty() && !is_meta {
                continue;
            }

            // Determine the list of formats to produce for this target.
            // If a `format_overrides[]` entry matches this OS, its `formats`
            // list takes priority. Otherwise, fall back to the archive's
            // own `formats` list (or the global default).
            let formats_to_produce: Vec<String> = {
                let (os, _arch) = map_target(target);
                // Format-override OS match via prefix: a FormatOverride.os
                // matches when the
                // resolved target's os field starts with the configured
                // value. Behavior matches `==` for canonical OS names
                // ("linux", "darwin", "windows"); the prefix relaxation
                // is exercised when an OS gains a sub-variant (e.g.,
                // "linux-musl") without users having to add an override.
                //
                // Empty `ov.os` is rejected as a typo guard (mirrors
                // `formats_for_target`): an accidental `os:` with no
                // value would otherwise match every target via empty-
                // prefix and silently override the archive's formats.
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
            };

            // Seed Os / Arch / Target plus the micro-architecture variant vars
            // (Arm / Arm64 / Amd64 / Mips / I386) the default name_template
            // reads. Shared with binstall/nix asset-name derivation so a
            // derived `pkg_url` cannot drift from the archive this stage writes.
            anodizer_core::archive_name::seed_target_vars(ctx, target);
            // Overlay the group's amd64 micro-arch level on top of the
            // target-derived baseline: a v3-tuned group (RUSTFLAGS
            // `-Ctarget-cpu=x86-64-v3`) must render the same `Amd64` suffix
            // its binaries were named with (`…_amd64v3.tar.gz`, matching
            // GoReleaser's default), and a user template's `{{ Amd64 }}`
            // must render the real level — the plain `seed_target_vars`
            // baseline would emit a FALSE "v1" for tuned groups. Same idiom
            // as the installer stages; the group carries one variant (the
            // first binary's, matching the metadata propagation below).
            let (_, group_arch) = map_target(target);
            anodizer_core::archive_name::seed_amd64_variant_var(
                ctx.template_vars_mut(),
                &group_arch,
                selected_bins
                    .first()
                    .and_then(|b| b.metadata.get("amd64_variant"))
                    .map(String::as_str),
            );
            let tvars = ctx.template_vars_mut();
            // CrateName is set per-crate so the multi-crate default
            // template (and any user template that references
            // `{{ .CrateName }}`) can produce distinct archive stems.
            tvars.set("CrateName", crate_name);

            // A binary carrying no `binary` metadata still has to name
            // itself: leaving `Binary` at the previous target's value renders
            // this target's stem from a different binary's name.
            if let Some(bin) = selected_bins.first() {
                tvars.set("Binary", &bin.binary_name().unwrap_or_default());
            }

            // Render name
            let archive_stem = ctx
                .render_template(name_tmpl)
                .with_context(|| format!("render archive name for {crate_name}/{target}"))?;
            // The rendered stem becomes the archive filename
            // (`{stem}.{format}` under `dist/`). An empty stem
            // produces a hidden file like `dist/.tar.gz` that
            // downstream stages (checksum, sign, release upload)
            // cannot resolve by canonical name. Bail with an
            // actionable hint instead of silently writing a
            // hidden artifact whose existence breaks the
            // duplicate-name detection a few lines below.
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
                    name_tmpl,
                    crate_name,
                    target
                );
            }

            // Render wrap_in_directory (template-aware)
            // WrapInDirectory::Bool(true)  -> use the archive stem as the wrap dir
            // WrapInDirectory::Bool(false) -> no wrapping
            // WrapInDirectory::Name(s)     -> treat as a template string to render
            let wrap_dir_rendered = if let Some(ref wid) = archive_cfg.wrap_in_directory {
                match wid {
                    anodizer_core::config::WrapInDirectory::Bool(true) => {
                        Some(archive_stem.clone())
                    }
                    anodizer_core::config::WrapInDirectory::Bool(false) => None,
                    anodizer_core::config::WrapInDirectory::Name(tmpl) => {
                        if tmpl.is_empty() {
                            None
                        } else {
                            Some(ctx.render_template(tmpl).with_context(|| {
                                format!("render wrap_in_directory for {crate_name}/{target}")
                            })?)
                        }
                    }
                }
            } else {
                None
            };
            // Reject path-traversal segments and absolute paths so a
            // user template cannot rewrite archive entries to an
            // arbitrary on-disk location once unpacked.
            if let Some(ref rendered) = wrap_dir_rendered
                && (rendered.contains("..") || Path::new(rendered).is_absolute())
            {
                bail!(
                    "archive: wrap_in_directory '{}' must be a relative path with no '..' segments",
                    rendered
                );
            }
            let wrap_dir = wrap_dir_rendered.as_deref();

            // Collect binary files — unless meta archive
            let mut binary_paths: Vec<PathBuf> = Vec::new();
            if !is_meta {
                for b in &selected_bins {
                    if !b.path.exists() && !dry_run {
                        anyhow::bail!(
                            "binary artifact missing: {} (expected at {})",
                            b.binary_name().unwrap_or_default(),
                            b.path.display()
                        );
                    }
                    binary_paths.push(b.path.clone());
                }
            }

            // A target producing nothing but `binary` outputs packs no extra
            // file, so resolving them is wasted IO — and under `--strict` an
            // unmatched `files:` glob would fail a release whose extras are
            // discarded either way.
            let binary_only_target = formats_to_produce.iter().all(|f| f == "binary");

            // Extra files (LICENSE, README, etc.) — with ArchiveFileSpec support.
            // When no files are configured, auto-include common files
            // (LICENSE*, README*, CHANGELOG*) default set.
            // File spec source patterns are rendered through the
            // template engine before glob expansion.
            let extra_files: Vec<ResolvedExtraFile> = if binary_only_target {
                Vec::new()
            } else if let Some(file_specs) = &archive_cfg.files {
                let rendered_specs: Vec<ArchiveFileSpec> = file_specs
                .iter()
                .map(|spec| -> Result<ArchiveFileSpec> {
                    Ok(match spec {
                        ArchiveFileSpec::Glob(pattern) => {
                            let rendered =
                                ctx.render_template(pattern).with_context(|| {
                                    format!(
                                        "archive: render files glob template '{pattern}' for {crate_name}/{target}"
                                    )
                                })?;
                            ArchiveFileSpec::Glob(rendered)
                        }
                        ArchiveFileSpec::Detailed {
                            src,
                            dst,
                            info,
                            strip_parent,
                        } => {
                            let rendered_src =
                                ctx.render_template(src).with_context(|| {
                                    format!(
                                        "archive: render files detailed src template '{src}' for {crate_name}/{target}"
                                    )
                                })?;
                            ArchiveFileSpec::Detailed {
                                src: rendered_src,
                                dst: dst.clone(),
                                info: info.clone(),
                                strip_parent: *strip_parent,
                            }
                        }
                    })
                })
                .collect::<Result<Vec<_>>>()?;
                resolve_file_specs(&rendered_specs, ctx.is_strict(), log)
                    .with_context(|| format!("resolve file specs for {crate_name}/{target}"))?
            } else {
                resolve_default_extra_files(crate_dir)
            };

            // Append the staged completion/man files (target-independent —
            // generated once above, reused for every target). They carry
            // their own archive `dst:` (e.g. `completions/rg.fish`) and pick
            // up the same `wrap_in_directory` prefix as user `files:`.
            let mut extra_files = extra_files;
            for aux in &aux_files {
                extra_files.push(ResolvedExtraFile {
                    src: aux.src.clone(),
                    dst: aux.dst.clone(),
                    info: aux.info.clone(),
                    strip_parent: aux.strip_parent,
                    default: aux.default,
                });
            }

            // builds_info: permissions applied to binary entries.
            // The build info mode is always forced to 0o755
            // when unset. Clone user's builds_info (or create default) and
            // ensure mode defaults to "0755" when None, preserving other
            // user-supplied fields (owner, group, mtime, etc).
            let mut binary_info = archive_cfg.builds_info.clone().unwrap_or_default();
            if binary_info.mode.is_none() {
                binary_info.mode = Some(anodizer_core::config::StringOrU32(0o755));
            }
            let binary_info = render_file_info(&binary_info, ctx)?;

            // Build ArchiveEntry items for binaries.
            // strip_binary_directory: when true, binaries skip the
            // wrap_in_directory prefix (placed at archive root).
            let binary_entries: Vec<ArchiveEntry> = binary_paths
                .iter()
                .map(|bp| {
                    let file_name = bp
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let raw_name = if strip_bin_dir {
                        PathBuf::from(&file_name)
                    } else if let Some(dir) = wrap_dir {
                        PathBuf::from(dir).join(&file_name)
                    } else {
                        PathBuf::from(&file_name)
                    };
                    let archive_name = formats::normalize_archive_path(raw_name);
                    ArchiveEntry {
                        src: bp.clone(),
                        archive_name,
                        info: Some(binary_info.clone()),
                    }
                })
                .collect();

            // Build ArchiveEntry items for extra files.
            // Extra files always get the wrap_in_directory prefix (if set).
            // When ArchiveFileSpec::Detailed has dst, use it as the
            // archive-internal name; apply per-file info permissions.
            let extra_entries: Vec<ArchiveEntry> = extra_files
                .iter()
                .map(|ef| -> Result<ArchiveEntry> {
                    let base_name = if let Some(ref dst) = ef.dst {
                        dst.clone()
                    } else if ef.strip_parent {
                        // strip_parent: use only the filename, discarding
                        // any parent directory components so the file ends
                        // up at the archive root (or directly under
                        // wrap_in_directory).
                        ef.src
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    } else {
                        // Use just the filename — extra files go at archive
                        // root (or under wrap_in_directory).
                        ef.src
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    };
                    let raw_name = if let Some(dir) = wrap_dir {
                        PathBuf::from(dir).join(&base_name)
                    } else {
                        PathBuf::from(&base_name)
                    };
                    let archive_name = formats::normalize_archive_path(raw_name);
                    Ok(ArchiveEntry {
                        src: ef.src.clone(),
                        archive_name,
                        info: ef
                            .info
                            .as_ref()
                            .map(|i| render_file_info(i, ctx))
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            // Record the in-archive paths of the bundled non-binary files
            // (LICENSE / README / CHANGELOG / completions / man) so the krew
            // publisher can emit a per-platform `files:` extraction list that
            // names exactly what the archive ships — and so the LICENSE/README
            // entries are gated on their actual presence rather than guessed.
            // These archive_names already carry the `wrap_in_directory` prefix,
            // which is precisely the `from:` shape krew's extractor needs for a
            // nested-archive layout.
            let archive_extra_files: Vec<String> = extra_entries
                .iter()
                .map(|e| e.archive_name.to_string_lossy().replace('\\', "/"))
                .collect();

            // Combine binary + extra entries and deduplicate by archive_name.
            // Deduplicate — first occurrence wins,
            // duplicates are warned and skipped.
            //
            // Per-archive `templated_files:` are appended INSIDE the
            // format loop below so each entry can see `.Format`, which
            // differs across tar.gz / zip / etc. for the same target.
            let base_entries: Vec<ArchiveEntry> =
                binary_entries.into_iter().chain(extra_entries).collect();

            let source_date_epoch: Option<u64> = resolve_archive_mtime(ctx);

            for format in &formats_to_produce {
                // "none" format: skip archive creation entirely for this target
                if format == "none" {
                    log.status(&format!(
                        "skipped archive for {crate_name}/{target} — format: none"
                    ));
                    continue;
                }

                // A meta entry carries no binaries, so `binary` has nothing to
                // emit for it. Say so instead of completing silently with an
                // empty dist/.
                if format == "binary" && selected_bins.is_empty() {
                    log.status(&format!(
                        "skipped archive for {crate_name}/{target} — meta archive \
                         under format: binary carries no binaries"
                    ));
                    continue;
                }

                // `binary` format produces ONE output per selected binary,
                // each named by rendering the name template with THAT
                // binary's `.Binary`; on Windows targets the executable
                // suffix is kept. Rendered up front so the collision guard,
                // the copy and the artifact registration agree on the paths.
                let binary_outputs: Vec<(String, PathBuf, &Artifact)> = if format == "binary" {
                    render_binary_outputs(
                        ctx,
                        &selected_bins,
                        binary_name_tmpl,
                        dist,
                        crate_name,
                        target,
                    )?
                } else {
                    Vec::new()
                };

                let archive_filename = if format == "binary" {
                    binary_outputs
                        .first()
                        .map(|(stem, _, _)| stem.clone())
                        .unwrap_or_default()
                } else {
                    format!("{archive_stem}.{format}")
                };
                let archive_path = dist.join(&archive_filename);

                // Expose `.Format` to per-archive templated_files (and
                // any downstream template that fires inside this scope).
                // Reset by `clear_archive_template_vars` after the
                // archive write completes.
                //
                // NOTE: the archive-identity vars (`.ArtifactName` /
                // `.ArtifactPath` / `.ArtifactExt` / `.ArtifactID`) are
                // deliberately NOT set on `ctx` here — `templated_files` render
                // below must see `.ArtifactPath` EMPTY (a templated file is an
                // INPUT to the archive and cannot reference the archive that
                // contains it; see `mode_a_*_does_not_leak` regression test).
                // They are overlaid onto the hook's var snapshot instead, just
                // below, so the `before:` hook still sees this archive's
                // identity without polluting the templated_files scope.
                ctx.template_vars_mut().set("Format", format);

                // Fire the `before:` hook here — after `.Format` / `.Os`
                // / `.Arch` / `.Target` are wired but before the archive
                // is written. Skipped for binary: the hook expects an archive
                // to post-process and this branch creates none.
                if format != "binary"
                    && let Some(pre) = archive_cfg.hooks.as_ref().and_then(|h| h.before.as_ref())
                {
                    // Overlay the archive-identity vars onto the hook's snapshot
                    // ONLY (not `ctx`), so a `before:` hook referencing
                    // `{{ .ArtifactPath }}` sees THIS archive's path — not a
                    // stale carry-over from the previous (target, format)
                    // iteration, and not an empty value — while the
                    // templated_files render below still sees them unset.
                    let mut hook_vars = ctx.template_vars().clone();
                    hook_vars.set("ArtifactName", &archive_filename);
                    hook_vars.set("ArtifactPath", &archive_path.to_string_lossy());
                    hook_vars.set(
                        "ArtifactExt",
                        anodizer_core::template::extract_artifact_ext(&archive_filename),
                    );
                    hook_vars.set("ArtifactID", archive_id);
                    run_hooks(
                        pre,
                        &pre_label,
                        HookRunContext::new(dry_run, log, Some(&hook_vars)),
                    )?;
                }

                // Render archive-scoped templated_files into a temp
                // staging dir, one tree per (archive_id, target, format).
                // Each rendered file becomes an `ArchiveEntry` packed
                // into the archive at its rendered `dst:` path. Skip
                // semantics + non-UTF8 input handling match the
                // top-level `template_files:` stage.
                //
                // A binary-only target packs nothing, so rendering these
                // would create a staging tree nobody reads and let a
                // template that cannot render fail a release over a file
                // the format discards.
                let templated_extra_entries = if binary_only_target {
                    Vec::new()
                } else {
                    render_archive_templated_files(
                        ctx,
                        archive_cfg.templated_files.as_deref().unwrap_or(&[]),
                        archive_id,
                        target,
                        format,
                        dist,
                    )?
                };

                // Combine entries, dedup, and sort. Repeated per format
                // because the templated_files set is format-specific.
                let mut combined: Vec<ArchiveEntry> = base_entries.to_vec();
                combined.extend(templated_extra_entries);
                let deduped = deduplicate_entries(combined);
                let sorted = sort_entries(deduped);
                let all_entries: Vec<&ArchiveEntry> = sorted.iter().collect();

                if is_meta && all_entries.is_empty() {
                    bail!(
                        "archive: meta archive for crate '{crate_name}' target '{target}' \
                     has zero files. Check your `files:` patterns — meta archives \
                     must bundle at least one file."
                    );
                }

                // For gz/binary formats, collect flat path refs.
                let all_src_paths: Vec<PathBuf> = sorted.iter().map(|e| e.src.clone()).collect();
                let path_refs: Vec<&Path> = all_src_paths.iter().map(PathBuf::as_path).collect();

                if format == "binary" {
                    // Extra files never travel with a raw binary: there is no
                    // container to put them in, and the consumer downloads one
                    // executable. On a binary-only target they were never
                    // resolved, so report what was configured rather than a
                    // count of entries that do not exist.
                    if binary_only_target {
                        let configured = archive_cfg.files.as_ref().is_some_and(|f| !f.is_empty())
                            || archive_cfg
                                .templated_files
                                .as_ref()
                                .is_some_and(|t| !t.is_empty());
                        if configured {
                            log.verbose(&format!(
                                "binary format ignores the files: entries for \
                                 crate '{crate_name}' target '{target}'"
                            ));
                        }
                    } else {
                        let ignored = sorted
                            .iter()
                            .filter(|e| !binary_paths.contains(&e.src))
                            .count();
                        if ignored > 0 {
                            log.verbose(&format!(
                                "binary format ignores {ignored} extra file(s) \
                                 for crate '{crate_name}' target '{target}'"
                            ));
                        }
                    }
                    for (stem, dest, bin) in &binary_outputs {
                        let replacing = claim_output_path(
                            name_guard,
                            dest,
                            "binary",
                            binary_name_tmpl,
                            stem,
                            crate_name,
                        )?;
                        if replacing && !dry_run {
                            log.verbose(&format!(
                                "replacing existing binary '{stem}' left by an earlier run"
                            ));
                        }
                        if dry_run {
                            log.status(&format!("(dry-run) would create {}", dest.display()));
                        } else {
                            log.status(&format!("creating {}", dest.display()));
                            formats::copy_binary(&bin.path, dest)?;
                        }
                    }
                } else {
                    let replacing = claim_output_path(
                        name_guard,
                        &archive_path,
                        "archive",
                        name_tmpl,
                        &archive_filename,
                        crate_name,
                    )?;
                    if replacing && !dry_run {
                        log.verbose(&format!(
                            "replacing existing archive '{archive_filename}' \
                             left by an earlier run"
                        ));
                    }
                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would create {} with {} files",
                            archive_path.display(),
                            all_entries.len()
                        ));
                    } else {
                        log.status(&format!("creating {}", archive_path.display()));
                        write_archive_in_format(
                            format,
                            &archive_path,
                            &all_entries,
                            &path_refs,
                            source_date_epoch,
                            ctx.is_strict(),
                            log,
                        )?;
                    }
                }

                // Now that the archive is written (and templated_files have
                // rendered with `.ArtifactPath` unset, per the input-not-output
                // contract above), seed the stage-scoped `.Artifact*` vars on
                // `ctx` so downstream stages and the `after:` hook resolve THIS
                // archive's identity.
                let tvars = ctx.template_vars_mut();
                tvars.set("ArtifactName", &archive_filename);
                tvars.set("ArtifactPath", &archive_path.to_string_lossy());
                tvars.set(
                    "ArtifactExt",
                    anodizer_core::template::extract_artifact_ext(&archive_filename),
                );
                tvars.set("ArtifactID", archive_id);

                let mut metadata = HashMap::from([
                    ("format".to_string(), format.clone()),
                    ("name".to_string(), archive_stem.clone()),
                    ("id".to_string(), archive_id.to_string()),
                ]);
                if is_meta {
                    metadata.insert("meta".to_string(), "true".to_string());
                }
                if strip_bin_dir {
                    metadata.insert("strip_binary_directory".to_string(), "true".to_string());
                }
                if let Some(dir) = wrap_dir {
                    metadata.insert("wrap_in_directory".to_string(), dir.to_string());
                }
                // Store binary names in archive metadata for publisher
                // consumption (e.g. Homebrew multi-binary install).
                // Sort the names so the joined string is byte-stable
                // across runs — selected_bins inherits its order from
                // the artifact registry, which can pick up HashMap
                // iteration order from earlier stages and surface as
                // mid-of-file drift in `artifacts.json`.
                let mut bin_names: Vec<String> = selected_bins
                    .iter()
                    .map(|b| b.binary_name().unwrap_or_default())
                    .collect();
                bin_names.sort();
                if !bin_names.is_empty() {
                    metadata.insert("extra_binaries".to_string(), bin_names.join(","));
                }
                // Record the bundled non-binary in-archive paths (LICENSE /
                // README / completions / man) so the krew publisher can emit a
                // `files:` extraction list gated on each file's actual presence.
                // `archive_extra_files` preserves the raw `extra_entries` order
                // (license / readme / changelog as `resolve_default_extra_files`
                // fixes it deterministically), collected BEFORE `deduplicate_entries`
                // — so it is NOT the archive's alphabetical `sort_entries` order.
                // Harmless for krew, which re-selects by basename rather than
                // relying on the list order.
                if !archive_extra_files.is_empty() && format != "binary" {
                    metadata.insert("archive_files".to_string(), archive_extra_files.join(","));
                }

                // propagate
                // Replaces + DynamicallyLinked + amd64_variant from
                // source binaries so publishers (Homebrew, AUR, nfpm,
                // winget, scoop, krew) can consume them.
                //   - Replaces: first non-empty value wins.
                //   - DynamicallyLinked (ndynlink): true if ANY source
                //     binary was dynamically linked, via the
                //     `DynamicallyLinked` extra
                //     is set when any source binary carries it.
                //   - amd64_variant: copied from the first source
                //     binary (the
                //     `art.Goamd64 = binaries[0].Goamd64`). Without
                //     this, publisher filters keyed on
                //     `metadata.get("amd64_variant")` fall back to the
                //     "missing == v1" default, so v2/v3/v4 archives
                //     would be matched as v1.
                let mut replaces_val: Option<String> = None;
                let mut any_dynlink = false;
                for b in &selected_bins {
                    if replaces_val.is_none()
                        && let Some(r) = b.metadata.get("replaces")
                        && !r.is_empty()
                    {
                        replaces_val = Some(r.clone());
                    }
                    // Match the canonical key (`DynamicallyLinked
                    // = "DynamicallyLinked"`) that the build stage
                    // writes via `is_dynamically_linked` detection.
                    if let Some(d) = b.metadata.get("DynamicallyLinked")
                        && d == "true"
                    {
                        any_dynlink = true;
                    }
                }
                if let Some(r) = replaces_val {
                    metadata.insert("replaces".to_string(), r);
                }
                if any_dynlink {
                    metadata.insert("ndynlink".to_string(), "true".to_string());
                }
                if let Some(variant) = selected_bins
                    .first()
                    .and_then(|b| b.metadata.get("amd64_variant"))
                {
                    metadata.insert("amd64_variant".to_string(), variant.clone());
                }

                if format == "binary" {
                    // `format=binary` emits one UploadableBinary artifact per
                    // source binary, not a single Archive: registering an
                    // Archive would point downstream stages
                    // (checksum/sign/release/blob) at a file that is never
                    // created on disk.
                    for (stem, dest, bin) in &binary_outputs {
                        let mut per_bin_meta = metadata.clone();
                        per_bin_meta
                            .insert("binary".to_string(), bin.binary_name().unwrap_or_default());
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::UploadableBinary,
                            name: stem.clone(),
                            path: dest.clone(),
                            target: Some(target.clone()),
                            crate_name: crate_name.to_string(),
                            metadata: per_bin_meta,
                            size: None,
                        });
                    }
                } else {
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        name: String::new(),
                        path: archive_path,
                        target: Some(target.clone()),
                        crate_name: crate_name.to_string(),
                        metadata,
                        size: None,
                    });
                }

                // Fire the `after:` hook here — after the archive is
                // written AND its `.ArtifactName` / `.ArtifactPath` /
                // `.ArtifactExt` / `.ArtifactID` vars are wired so the
                // hook can reference the freshly-built archive (e.g.,
                // `cosign sign-blob {{ ArtifactPath }}`). Skipped for
                // `binary`.
                if format != "binary"
                    && let Some(post) = archive_cfg.hooks.as_ref().and_then(|h| h.after.as_ref())
                {
                    let hook_vars = ctx.template_vars().clone();
                    run_hooks(
                        post,
                        &post_label,
                        HookRunContext::new(dry_run, log, Some(&hook_vars)),
                    )?;
                }
            }
        }
    }
    Ok(())
}
