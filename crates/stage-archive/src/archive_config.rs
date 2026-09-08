//! Execute pass of the archive stage: writes the outputs a [`CratePlan`]
//! resolved, split from `run.rs`.
//!
//! Holds [`write_crate_archives`], the heaviest helper the
//! [`crate::ArchiveStage`] driver calls into: it produces every archive (or
//! per-binary output) the plan holds for one crate. Name resolution and the
//! output-path claims happened in the planning pass (`plan.rs`), so nothing
//! here can refuse over a name — by the time this runs, every path of the
//! whole run is known to be distinct.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::ArchiveFileSpec;
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anyhow::{Context as _, Result, bail};

use crate::entries::{ArchiveEntry, deduplicate_entries, sort_entries};
use crate::file_specs::{
    ResolvedExtraFile, render_file_info, resolve_default_extra_files, resolve_file_specs,
};
use crate::formats;
use crate::plan::{CratePlan, seed_target_context};
use crate::run::resolve_host_binary;
use crate::run_helpers::{
    render_archive_templated_files, resolve_archive_mtime, write_archive_in_format,
};

/// Write every output the plan holds for one crate, appending one `Archive`
/// (or per-binary `UploadableBinary`) artifact per produced output to
/// `new_artifacts`.
pub(crate) fn write_crate_archives(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    dist: &Path,
    dry_run: bool,
    plan: &CratePlan,
    new_artifacts: &mut Vec<Artifact>,
) -> Result<()> {
    let crate_name = plan.crate_name.as_str();
    let crate_dir = plan.crate_dir.as_path();
    for cfg_plan in &plan.configs {
        if let Some(note) = &cfg_plan.skip {
            log.status(note);
            continue;
        }
        let archive_cfg = &plan.archive_cfgs[cfg_plan.index];
        let archive_id = cfg_plan.archive_id.as_str();
        let is_meta = cfg_plan.is_meta;

        // strip_binary_directory: place binaries at archive root
        let strip_bin_dir = archive_cfg.strip_binary_directory.unwrap_or(false);

        // Generate (or harvest/copy) completion + man files ONCE for this
        // archive config and stage them in dist so the SAME files feed both
        // the archive and nfpm `contents:` globs. Mode A reuses the
        // host-native binary's output for every target (arch-independent).
        let aux_files: Vec<ResolvedExtraFile> =
            if archive_cfg.completions.is_some() || archive_cfg.manpages.is_some() {
                let host_binary = resolve_host_binary(&plan.all_binaries);
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

        for target_plan in &cfg_plan.targets {
            let target = target_plan.target.as_str();
            let selected_bins: Vec<&Artifact> = target_plan.selected_bins.iter().collect();
            seed_target_context(
                ctx,
                target,
                crate_name,
                &target_plan.selected_bins,
                target_plan.group_variant.as_deref(),
            );
            let archive_stem = target_plan.archive_stem.as_str();

            // Render wrap_in_directory (template-aware)
            // WrapInDirectory::Bool(true)  -> use the archive stem as the wrap dir
            // WrapInDirectory::Bool(false) -> no wrapping
            // WrapInDirectory::Name(s)     -> treat as a template string to render
            let wrap_dir_rendered = if let Some(ref wid) = archive_cfg.wrap_in_directory {
                match wid {
                    anodizer_core::config::WrapInDirectory::Bool(true) => {
                        Some(archive_stem.to_string())
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
            let binary_only_target = target_plan.binary_only;

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
                    } else {
                        // Either way the file lands at the archive root (or
                        // directly under wrap_in_directory): `strip_parent`
                        // drops the parent components, and a bare glob match
                        // keeps only its file name.
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

            for format_plan in &target_plan.formats {
                if let Some(note) = &format_plan.skip {
                    log.status(note);
                    continue;
                }
                let format = format_plan.format.as_str();
                let archive_filename = format_plan.archive_filename.as_str();
                let archive_path = format_plan.archive_path.as_path();

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
                    hook_vars.set("ArtifactName", archive_filename);
                    hook_vars.set("ArtifactPath", &archive_path.to_string_lossy());
                    hook_vars.set(
                        "ArtifactExt",
                        anodizer_core::template::extract_artifact_ext(archive_filename),
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
                    for out in &format_plan.binary_outputs {
                        if out.replacing && !dry_run {
                            log.verbose(&format!(
                                "replacing existing binary '{}' left by an earlier run",
                                out.stem
                            ));
                        }
                        if dry_run {
                            log.status(&format!("(dry-run) would create {}", out.dest.display()));
                        } else {
                            log.status(&format!("creating {}", out.dest.display()));
                            formats::copy_binary(&out.bin.path, &out.dest)?;
                        }
                    }
                } else {
                    if format_plan.replacing && !dry_run {
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
                            archive_path,
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
                tvars.set("ArtifactName", archive_filename);
                tvars.set("ArtifactPath", &archive_path.to_string_lossy());
                tvars.set(
                    "ArtifactExt",
                    anodizer_core::template::extract_artifact_ext(archive_filename),
                );
                tvars.set("ArtifactID", archive_id);

                let metadata = archive_metadata(
                    format,
                    archive_stem,
                    archive_id,
                    is_meta,
                    strip_bin_dir,
                    wrap_dir,
                    &selected_bins,
                    &archive_extra_files,
                );

                if format == "binary" {
                    // `format=binary` emits one UploadableBinary artifact per
                    // source binary, not a single Archive: registering an
                    // Archive would point downstream stages
                    // (checksum/sign/release/blob) at a file that is never
                    // created on disk.
                    for out in &format_plan.binary_outputs {
                        let mut per_bin_meta = metadata.clone();
                        per_bin_meta.insert(
                            "binary".to_string(),
                            out.bin.binary_name().unwrap_or_default(),
                        );
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::UploadableBinary,
                            name: out.stem.clone(),
                            path: out.dest.clone(),
                            target: Some(target.to_string()),
                            crate_name: crate_name.to_string(),
                            metadata: per_bin_meta,
                            size: None,
                        });
                    }
                } else {
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Archive,
                        name: String::new(),
                        path: archive_path.to_path_buf(),
                        target: Some(target.to_string()),
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

/// The metadata every artifact of one (target, format) carries: the format,
/// name and id, the layout flags, the sorted binary names, the bundled
/// non-binary paths, and the values publishers consume from the source
/// binaries (`replaces`, `ndynlink`, `amd64_variant`).
#[allow(clippy::too_many_arguments)]
fn archive_metadata(
    format: &str,
    archive_stem: &str,
    archive_id: &str,
    is_meta: bool,
    strip_bin_dir: bool,
    wrap_dir: Option<&str>,
    selected_bins: &[&Artifact],
    archive_extra_files: &[String],
) -> HashMap<String, String> {
    let mut metadata = HashMap::from([
        ("format".to_string(), format.to_string()),
        ("name".to_string(), archive_stem.to_string()),
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
    // Sorted so the joined string is byte-stable across runs: selected_bins
    // inherits its order from the artifact registry, which can pick up
    // HashMap iteration order from earlier stages and surface as
    // mid-of-file drift in `artifacts.json`.
    let mut bin_names: Vec<String> = selected_bins
        .iter()
        .map(|b| b.binary_name().unwrap_or_default())
        .collect();
    bin_names.sort();
    if !bin_names.is_empty() {
        metadata.insert("extra_binaries".to_string(), bin_names.join(","));
    }
    // The bundled non-binary in-archive paths, so the krew publisher can emit
    // a `files:` extraction list gated on each file's actual presence. Kept
    // in the raw `extra_entries` order, not the archive's sorted order;
    // krew re-selects by basename and never relies on the order.
    if !archive_extra_files.is_empty() && format != "binary" {
        metadata.insert("archive_files".to_string(), archive_extra_files.join(","));
    }

    // Publisher-facing values propagated from the source binaries:
    //   - replaces: first non-empty value wins;
    //   - ndynlink: true when ANY source binary was dynamically linked;
    //   - amd64_variant: copied from the first source binary, since filters
    //     keyed on the key treat a missing value as the v1 baseline and
    //     would match a v3 archive as v1.
    let mut replaces_val: Option<String> = None;
    let mut any_dynlink = false;
    for b in selected_bins {
        if replaces_val.is_none()
            && let Some(r) = b.metadata.get("replaces")
            && !r.is_empty()
        {
            replaces_val = Some(r.clone());
        }
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
    metadata
}
