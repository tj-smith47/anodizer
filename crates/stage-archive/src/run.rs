use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::{
    ArchiveConfig, ArchiveFileSpec, ArchivesConfig, FormatOverride, VALID_ARCHIVE_FORMATS,
};
use anodizer_core::context::Context;
use anodizer_core::hooks::run_hooks;
use anodizer_core::stage::Stage;
use anodizer_core::target::map_target;

use crate::entries::{
    ArchiveEntry, deduplicate_entries, sort_entries, write_archive_entries, write_zip_entries,
};
use crate::file_specs::{
    ResolvedExtraFile, render_file_info, resolve_default_extra_files, resolve_file_specs,
};
use crate::formats::{self, copy_binary, create_gz, create_xz};
use crate::{
    ArchiveStage, default_binary_name_template, default_name_template,
    default_name_template_multi_crate,
};

impl Stage for ArchiveStage {
    fn name(&self) -> &str {
        "archive"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("archive");
        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let template_vars = ctx.template_vars().clone();

        // Global archive defaults: first entry of `defaults.archives.formats`
        // is the singular default; falls back to "tar.gz" when neither is set.
        let global_default_format = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.formats.as_ref())
            .and_then(|fmts| fmts.first().cloned())
            .unwrap_or_else(|| "tar.gz".to_string());
        let global_format_overrides: Vec<FormatOverride> = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.format_overrides.clone())
            .unwrap_or_default();

        // Collect crate configs to avoid borrow conflict later
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Build a list of (crate_name, archive_configs) pairs to process.
        //
        // A crate enters the archive workflow only when it has *something to
        // archive*: either configured builds, a meta-archive (which doesn't
        // need binaries), or already-registered binary artifacts inherited from a
        // prior stage / split-merge context. Library-only crates with none of
        // the above are silently skipped — treating "no binaries" as a
        // strict-mode error for them would force every lib-only crate in a
        // workspace to opt out via `archives: disabled`. The strict_guard
        // further down still fires when a crate qualified to enter the
        // workflow but produced no binaries — that's the genuine error case.
        let archivable_kinds = [
            ArtifactKind::Binary,
            ArtifactKind::UniversalBinary,
            ArtifactKind::Header,
            ArtifactKind::CArchive,
            ArtifactKind::CShared,
        ];
        // Resolve the project root once: per-crate paths in CrateConfig are
        // recorded relative to the project root, so default-extra-files glob
        // (LICENSE/README/CHANGELOG) needs an absolute base to avoid leaking
        // the workspace's own README into per-crate archives during
        // `cargo test` runs that execute from the workspace root.
        let project_root = ctx
            .options
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let work: Vec<(String, PathBuf, Vec<ArchiveConfig>)> = crates
            .into_iter()
            .filter_map(|c| {
                match &c.archives {
                    ArchivesConfig::Disabled => None,
                    ArchivesConfig::Configs(cfgs) => {
                        let has_builds = c.builds.as_ref().map(|b| !b.is_empty()).unwrap_or(false);
                        let has_meta_archive = cfgs.iter().any(|cfg| cfg.meta.unwrap_or(false));
                        let has_existing_artifacts = !ctx
                            .artifacts
                            .by_kinds_and_crate(&archivable_kinds, &c.name)
                            .is_empty();
                        if !has_builds && !has_meta_archive && !has_existing_artifacts {
                            return None;
                        }
                        let archive_cfgs = if cfgs.is_empty() {
                            // Default: one archive with all defaults
                            vec![ArchiveConfig::default()]
                        } else {
                            cfgs.clone()
                        };
                        let crate_dir = project_root.join(&c.path);
                        Some((c.name.clone(), crate_dir, archive_cfgs))
                    }
                }
            })
            .collect();

        // Early validation: reject unknown archive format strings before doing
        // any I/O so typos are surfaced immediately.
        for (_crate_name, _crate_dir, archive_cfgs) in &work {
            for cfg in archive_cfgs {
                if let Some(ref fmts) = cfg.formats {
                    for fmt in fmts {
                        if !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str()) {
                            bail!(
                                "unsupported archive format: {fmt} (valid: {})",
                                VALID_ARCHIVE_FORMATS.join(", ")
                            );
                        }
                    }
                }
                if let Some(ref overrides) = cfg.format_overrides {
                    for ov in overrides {
                        // GoReleaser warns when format_overrides entries have
                        // empty goos or empty format.
                        if ov.os.is_empty() {
                            log.warn("format_override has empty goos/os value");
                        }
                        if ov.formats.as_ref().is_none_or(|f| f.is_empty()) {
                            log.warn("format_override has empty formats value");
                        }
                        if let Some(ref fmts) = ov.formats {
                            for fmt in fmts {
                                if !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str()) {
                                    bail!(
                                        "unsupported archive format: {fmt} (valid: {})",
                                        VALID_ARCHIVE_FORMATS.join(", ")
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Ensure dist directory exists
        fs::create_dir_all(&dist)
            .with_context(|| format!("create dist dir: {}", dist.display()))?;

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        // pick the default `archive.name_template` based on whether
        // the run produces archives for more than one crate. In a single-crate
        // config the GoReleaser-canonical `{{ .ProjectName }}_..._{{ .Os }}_..`
        // template is unambiguous; in a monorepo every crate would resolve to
        // the same filename and emit `artifact '<...>' already registered`
        // warnings.
        //
        // In multi-crate mode the default template still references
        // `{{ .ProjectName }}` (matching GR archive.go:30 verbatim) and the
        // per-crate iteration below overrides the `ProjectName` template var
        // to the crate name. This keeps user templates that reference
        // `{{ .ProjectName }}` working under GR semantics — each crate is its
        // own "project" in the workspace model. `{{ .CrateName }}` remains
        // separately available.
        let multi_crate = work.len() > 1;

        // Snapshot the workspace-level ProjectName so the per-crate override
        // can be restored after the loop (downstream stages — checksum, sign,
        // release, blob — must continue to see the workspace name).
        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| ctx.config.project_name.clone());

        for (crate_name, crate_dir, archive_cfgs) in &work {
            // GR-aligned ProjectName resolution for multi-crate workspaces:
            // in a single-crate config ProjectName == workspace project name;
            // in a multi-crate config each crate behaves like its own project,
            // so user `name_template`s referencing `{{ .ProjectName }}` (the
            // common GR migration shape) resolve to a per-crate-distinct
            // value instead of the workspace name.
            if multi_crate {
                ctx.template_vars_mut().set("ProjectName", crate_name);
            }
            // Archive all build artifact types, matching GoReleaser
            // (Binary, UniversalBinary, Header, CArchive, CShared).
            let archivable_kinds = [
                ArtifactKind::Binary,
                ArtifactKind::UniversalBinary,
                ArtifactKind::Header,
                ArtifactKind::CArchive,
                ArtifactKind::CShared,
            ];
            let all_binaries: Vec<Artifact> = ctx
                .artifacts
                .by_kinds_and_crate(&archivable_kinds, crate_name)
                .into_iter()
                .cloned()
                .collect();

            // meta archives can skip the "no binaries" check
            let has_any_meta = archive_cfgs.iter().any(|cfg| cfg.meta.unwrap_or(false));

            if all_binaries.is_empty() && !has_any_meta {
                ctx.strict_guard(
                    &log,
                    &format!("no binaries for crate {crate_name}, skipping"),
                )?;
                continue;
            }

            for archive_cfg in archive_cfgs {
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
                    all_binaries.clone()
                };

                if binaries.is_empty() && !is_meta {
                    let archive_id = archive_cfg.id.as_deref().unwrap_or("default");
                    let id_filter_desc = archive_cfg
                        .ids
                        .as_deref()
                        .map(|ids| format!(" matching ids {ids:?}"))
                        .unwrap_or_default();
                    log.warn(&format!(
                        "archive[{archive_id}]: crate {crate_name} has no binaries{id_filter_desc} \
                         — skipping (set `meta: true` if this is intentional)"
                    ));
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
                // (matches GoReleaser behavior which errors, not warns).
                // GoReleaser archive/archive.go:129 exempts the "binary"
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

                // Determine singular default format (per-config first entry > global default)
                let singular_format = archive_cfg
                    .formats
                    .as_ref()
                    .and_then(|fs| fs.first().map(|s| s.as_str()))
                    .unwrap_or(&global_default_format);

                // Determine format overrides (per-config > global)
                let format_overrides: Vec<FormatOverride> = archive_cfg
                    .format_overrides
                    .clone()
                    .unwrap_or_else(|| global_format_overrides.clone());

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

                // strip_binary_directory: place binaries at archive root
                let strip_bin_dir = archive_cfg.strip_binary_directory.unwrap_or(false);

                // Archive-scope template vars expose `.Format` to hook templates.
                let mut hook_vars = template_vars.clone();
                hook_vars.set("Format", singular_format);

                let archive_id = archive_cfg.id.as_deref().unwrap_or("default");
                let pre_label = format!("pre-archive[{archive_id}]");
                let post_label = format!("post-archive[{archive_id}]");

                if let Some(pre) = archive_cfg.hooks.as_ref().and_then(|h| h.before.as_ref()) {
                    run_hooks(pre, &pre_label, dry_run, &log, Some(&hook_vars))?;
                }

                for (target, target_bins) in &by_target {
                    // Filter binaries for this archive config
                    let selected_bins: Vec<&Artifact> = target_bins
                        .iter()
                        .filter(|b| match binary_filter {
                            None => true,
                            Some(names) => {
                                let bin_name =
                                    b.metadata.get("binary").map(|s| s.as_str()).unwrap_or("");
                                names.iter().any(|n| n == bin_name)
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
                        // GR-aligned (archive.go:349 `strings.HasPrefix(platform,
                        // override.Goos)`): a FormatOverride.os matches when the
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
                                .unwrap_or_else(|| vec![global_default_format.clone()]),
                        }
                    };

                    let (os, arch) = map_target(target);

                    // Build template vars for this target
                    let tvars = ctx.template_vars_mut();
                    tvars.set("Os", &os);
                    tvars.set("Arch", &arch);
                    tvars.set("Target", target);
                    // CrateName is set per-crate so the multi-crate default
                    // template (and any user template that references
                    // `{{ .CrateName }}`) can produce distinct archive stems.
                    tvars.set("CrateName", crate_name);

                    // Set Binary to the first selected binary's name (matches GoReleaser behavior)
                    if let Some(bin_name) =
                        selected_bins.first().and_then(|b| b.metadata.get("binary"))
                    {
                        tvars.set("Binary", bin_name);
                    }

                    // Render name
                    let archive_stem = ctx.render_template(name_tmpl).with_context(|| {
                        format!("render archive name for {crate_name}/{target}")
                    })?;

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
                                        format!(
                                            "render wrap_in_directory for {crate_name}/{target}"
                                        )
                                    })?)
                                }
                            }
                        }
                    } else {
                        None
                    };
                    let wrap_dir = wrap_dir_rendered.as_deref();

                    // Collect binary files — unless meta archive
                    let mut binary_paths: Vec<PathBuf> = Vec::new();
                    if !is_meta {
                        for b in &selected_bins {
                            if !b.path.exists() && !dry_run {
                                anyhow::bail!(
                                    "binary artifact missing: {} (expected at {})",
                                    b.metadata.get("binary").unwrap_or(&b.crate_name),
                                    b.path.display()
                                );
                            }
                            binary_paths.push(b.path.clone());
                        }
                    }

                    // Extra files (LICENSE, README, etc.) — with ArchiveFileSpec support.
                    // When no files are configured, auto-include common files
                    // (LICENSE*, README*, CHANGELOG*) matching GoReleaser defaults.
                    // GoReleaser renders file spec source patterns through the
                    // template engine before glob expansion.
                    let extra_files: Vec<ResolvedExtraFile> = if let Some(file_specs) =
                        &archive_cfg.files
                    {
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
                        resolve_file_specs(&rendered_specs).with_context(|| {
                            format!("resolve file specs for {crate_name}/{target}")
                        })?
                    } else {
                        resolve_default_extra_files(crate_dir)
                    };

                    // builds_info: permissions applied to binary entries.
                    // GoReleaser archive.go:99 always forces BuildsInfo.Mode = 0o755
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

                    // Combine binary + extra entries and deduplicate by archive_name.
                    // Matches GoReleaser's unique() — first occurrence wins,
                    // duplicates are warned and skipped.
                    let combined: Vec<ArchiveEntry> =
                        binary_entries.into_iter().chain(extra_entries).collect();
                    let deduped = deduplicate_entries(combined);
                    let sorted = sort_entries(deduped);
                    let all_entries: Vec<&ArchiveEntry> = sorted.iter().collect();

                    // a meta archive must have
                    // at least one file. Silently emitting an empty archive masks
                    // a config bug (user set `meta: true` but no `files:` patterns
                    // matched). Hard-error with a clear message.
                    if is_meta && all_entries.is_empty() {
                        bail!(
                            "archive: meta archive for crate '{crate_name}' target '{target}' \
                             has zero files. Check your `files:` patterns — meta archives \
                             must bundle at least one file."
                        );
                    }

                    // For gz/binary formats, collect flat path refs (these formats
                    // don't support per-entry metadata)
                    let all_src_paths: Vec<PathBuf> =
                        sorted.iter().map(|e| e.src.clone()).collect();
                    let path_refs: Vec<&Path> =
                        all_src_paths.iter().map(PathBuf::as_path).collect();

                    // Determine reproducible mtime. Priority:
                    //   1. `reproducible: true` on any crate build → CommitTimestamp
                    //      (explicit opt-in for full Rust reproducibility).
                    //   2. SOURCE_DATE_EPOCH env var → use that (standard external override).
                    //   3. CommitTimestamp fallback → deterministic by default.
                    //
                    // The fallback is load-bearing for release-asset idempotency:
                    // anodizer-action's outer retry wrapper re-runs the pipeline on
                    // transient downstream failures (e.g. snapcraft cache race,
                    // crates.io 429). Without a stable mtime, each re-run embeds the
                    // download-artifact extraction time into each archive entry,
                    // producing byte-divergent archives. GitHub's ReleaseAsset API then
                    // rejects the re-upload with `already_exists` (size mismatch),
                    // defeating stage-release's size-based idempotency path.
                    //
                    // If neither CommitTimestamp nor SOURCE_DATE_EPOCH is available
                    // (e.g. non-git snapshot outside a workflow), `None` falls back to
                    // filesystem mtime — preserves prior behavior for that edge case.
                    let source_date_epoch: Option<u64> = {
                        let any_reproducible = ctx.config.crates.iter().any(|c| {
                            c.builds.as_ref().is_some_and(|builds| {
                                builds.iter().any(|b| b.reproducible.unwrap_or(false))
                            })
                        });
                        let commit_ts = ctx
                            .template_vars()
                            .get("CommitTimestamp")
                            .and_then(|ts| ts.parse::<u64>().ok());
                        if any_reproducible {
                            commit_ts
                        } else {
                            std::env::var("SOURCE_DATE_EPOCH")
                                .ok()
                                .and_then(|s| s.parse::<u64>().ok())
                                .or(commit_ts)
                        }
                    };

                    for format in &formats_to_produce {
                        // "none" format: skip archive creation entirely for this target
                        if format == "none" {
                            log.status(&format!(
                                "skipping archive for {crate_name}/{target} (format: none)"
                            ));
                            continue;
                        }

                        // For binary format, no extension by default; on Windows
                        // targets append `.exe` to match upstream GoReleaser
                        // (archive.go:298-302 — Windows binaries keep their
                        // executable suffix even in binary-format archives).
                        // For non-binary formats, append the format as the extension.
                        // GoReleaser uses {{ .Binary }} prefix (not {{ .ProjectName }})
                        // for binary format when no custom name_template is set.
                        let archive_filename = if format == "binary" {
                            let stem = if has_custom_name_tmpl {
                                archive_stem.clone()
                            } else {
                                ctx.render_template(default_binary_name_template())
                                    .with_context(|| {
                                        format!(
                                            "archive: render default binary name template for {crate_name}/{target}"
                                        )
                                    })?
                            };
                            if anodizer_core::target::is_windows(target) && !stem.ends_with(".exe")
                            {
                                format!("{stem}.exe")
                            } else {
                                stem
                            }
                        } else {
                            format!("{archive_stem}.{format}")
                        };
                        let archive_path = dist.join(&archive_filename);

                        // Duplicate archive name detection: prevent silent overwrites
                        if archive_path.exists() {
                            bail!(
                                "archive named '{}' already exists. Check your archive name template.",
                                archive_filename
                            );
                        }

                        if dry_run {
                            log.status(&format!(
                                "(dry-run) would create {} with {} files",
                                archive_path.display(),
                                all_entries.len()
                            ));
                        } else {
                            log.status(&format!("creating {}", archive_path.display()));
                            match format.as_str() {
                                "zip" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create zip: {}", archive_path.display())
                                        })?;
                                    let mut zip = zip::ZipWriter::new(out_file);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_zip_entries(&mut zip, &owned, source_date_epoch)?;
                                    zip.finish().context("zip: finish")?;
                                }
                                "tar.gz" | "tgz" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.gz: {}", archive_path.display())
                                        })?;
                                    let enc = GzEncoder::new(out_file, Compression::best());
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.gz",
                                    )?;
                                    tar.finish().context("tar.gz: finish")?;
                                }
                                "tar.xz" | "txz" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.xz: {}", archive_path.display())
                                        })?;
                                    let enc = xz2::write::XzEncoder::new(out_file, 9);
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.xz",
                                    )?;
                                    tar.finish().context("tar.xz: finish")?;
                                }
                                "tar.zst" | "tzst" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.zst: {}", archive_path.display())
                                        })?;
                                    let enc = zstd::Encoder::new(out_file, 3)
                                        .context("tar.zst: create zstd encoder")?;
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.zst",
                                    )?;
                                    let enc = tar.into_inner().context("tar.zst: finish tar")?;
                                    enc.finish().context("tar.zst: finish zstd")?;
                                }
                                "tar" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar: {}", archive_path.display())
                                        })?;
                                    let mut tar = tar::Builder::new(out_file);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar",
                                    )?;
                                    tar.finish().context("tar: finish")?;
                                }
                                "gz" => {
                                    if path_refs.is_empty() {
                                        bail!("gz format requires at least one file");
                                    }
                                    if path_refs.len() > 1 {
                                        log.warn(&format!(
                                            "gz format only compresses a single file; {} extra files will be skipped",
                                            path_refs.len() - 1
                                        ));
                                    }
                                    create_gz(path_refs[0], &archive_path)?;
                                }
                                "xz" => {
                                    // Mirrors GoReleaser commit bb532b6
                                    // (pkg/archive/xz/xz.go): xz is a
                                    // single-file format. Multiple inputs
                                    // are a hard error, not a warning —
                                    // upstream returns
                                    // `xz: failed to add %s, only one file
                                    // can be archived in xz format`.
                                    if path_refs.is_empty() {
                                        bail!("xz format requires exactly one file");
                                    }
                                    if path_refs.len() > 1 {
                                        bail!(
                                            "xz: failed to add {}, only one file can be archived in xz format",
                                            path_refs[1].display()
                                        );
                                    }
                                    create_xz(path_refs[0], &archive_path)?;
                                }
                                "binary" => copy_binary(&path_refs, &archive_path)?,
                                _ => bail!("unsupported archive format: {format}"),
                            }
                        }

                        // Update stage-scoped template vars for downstream stages
                        let tvars = ctx.template_vars_mut();
                        tvars.set("ArtifactName", &archive_filename);
                        tvars.set("ArtifactPath", &archive_path.to_string_lossy());
                        tvars.set(
                            "ArtifactExt",
                            anodizer_core::template::extract_artifact_ext(&archive_filename),
                        );
                        // GoReleaser archive Default() sets ID="default" when empty.
                        // Downstream `ids:` filters rely on this to match unlabeled archives.
                        let archive_id = archive_cfg.id.as_deref().unwrap_or("default");
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
                            metadata
                                .insert("strip_binary_directory".to_string(), "true".to_string());
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
                            .filter_map(|b| {
                                b.metadata.get("binary").cloned().or_else(|| {
                                    b.path.file_name().map(|n| n.to_string_lossy().to_string())
                                })
                            })
                            .collect();
                        bin_names.sort();
                        if !bin_names.is_empty() {
                            metadata.insert("extra_binaries".to_string(), bin_names.join(","));
                        }

                        // propagate
                        // Replaces + DynamicallyLinked + amd64_variant from
                        // source binaries so publishers (Homebrew, AUR, nfpm,
                        // winget, scoop, krew) can consume them.
                        //   - Replaces: first non-empty value wins.
                        //   - DynamicallyLinked (ndynlink): true if ANY source
                        //     binary was dynamically linked. Mirrors GR
                        //     archive.go:266-270 where `art.Extra[ExtranDynLink]`
                        //     is set when any source binary carries it.
                        //   - amd64_variant: copied from the first source
                        //     binary (mirrors GR archive.go:255
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
                            // Match the canonical GoReleaser key (`ExtranDynLink
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
                            // GoReleaser archive.go:143-145,296-336: `format=binary`
                            // emits one UploadableBinary artifact per source
                            // binary, not a single Archive. Registering an Archive
                            // with the "parent" archive_path would point downstream
                            // stages (checksum/sign/release/blob) at a file that is
                            // never created on disk.
                            let out_dir = archive_path.parent().unwrap_or(Path::new("."));
                            for bin in &selected_bins {
                                let file_name = bin
                                    .path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| bin.path.to_string_lossy().to_string());
                                let dest = if path_refs.len() == 1 {
                                    archive_path.clone()
                                } else {
                                    out_dir.join(&file_name)
                                };
                                let mut per_bin_meta = metadata.clone();
                                if let Some(bin_name) = bin.metadata.get("binary") {
                                    per_bin_meta.insert("binary".to_string(), bin_name.clone());
                                }
                                new_artifacts.push(Artifact {
                                    kind: ArtifactKind::UploadableBinary,
                                    name: file_name,
                                    path: dest,
                                    target: Some(target.clone()),
                                    crate_name: crate_name.clone(),
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
                                crate_name: crate_name.clone(),
                                metadata,
                                size: None,
                            });
                        }
                    }
                }

                if let Some(post) = archive_cfg.hooks.as_ref().and_then(|h| h.after.as_ref()) {
                    run_hooks(post, &post_label, dry_run, &log, Some(&hook_vars))?;
                }
            }
        }

        // Restore the workspace-level ProjectName after multi-crate iteration
        // overrode it per-crate (see Q-arch1 / 2026-05-08 second-opinion audit).
        // Downstream stages (checksum, sign, release, blob) must see the
        // workspace project name, not whichever crate happened to be last.
        ctx.template_vars_mut()
            .set("ProjectName", &original_project_name);

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Target", "");
        ctx.template_vars_mut().set("Binary", "");
        ctx.template_vars_mut().set("ArtifactName", "");
        ctx.template_vars_mut().set("ArtifactPath", "");
        ctx.template_vars_mut().set("ArtifactExt", "");
        ctx.template_vars_mut().set("ArtifactID", "");

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}
