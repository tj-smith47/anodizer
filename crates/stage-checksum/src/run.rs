//! `ChecksumStage` + the post-sign `refresh_combined_checksums` helper.
//!
//! The stage hashes every checksummable artifact for each crate, writes
//! either per-artifact sidecars (split mode) or a combined `checksums.txt`
//! per crate, and propagates `<algo> -> hex` back into source-artifact
//! metadata so downstream publishers (homebrew, scoop, krew, ...) can
//! reach it without re-hashing.
//!
//! `refresh_combined_checksums` is invoked by `stage-release` after signing
//! to rewrite combined files so signature artifacts that happen to be
//! uploadable land in the final sums (the refresh hook).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::{
    ChecksumConfig, ChecksumSplitFormat, CrateConfig, ExtraFileSpec, TemplatedExtraFile,
};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;

use super::hashing::{format_checksum_line, hash_file, hash_hex_len, validate_algorithm};

/// Per-crate resolved checksum settings, merging global defaults with the
/// crate's own overrides.
struct ResolvedChecksumConfig {
    algorithm: String,
    name_template: Option<String>,
    extra_files: Option<Vec<ExtraFileSpec>>,
    templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    ids_filter: Option<Vec<String>>,
    split: bool,
    split_format: ChecksumSplitFormat,
}

/// Global checksum defaults pulled from `defaults.checksum`.
struct GlobalChecksumDefaults {
    algorithm: String,
    name_template: Option<String>,
    extra_files: Option<Vec<ExtraFileSpec>>,
    templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    ids: Option<Vec<String>>,
    split: Option<bool>,
    split_format: Option<ChecksumSplitFormat>,
}

// ---------------------------------------------------------------------------
// Extra-files glob resolution
// ---------------------------------------------------------------------------

/// Resolved extra file: the path on disk and an optional name_template override.
struct ResolvedExtraFile {
    path: PathBuf,
    name_template: Option<String>,
}

/// Resolve `extra_files` via the canonical `core::extrafiles::resolve` — thin
/// adapter that returns the local `ResolvedExtraFile` shape expected by the
/// rest of this module.
fn resolve_extra_files(
    specs: &[ExtraFileSpec],
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<ResolvedExtraFile>> {
    anodizer_core::extrafiles::resolve(specs, log)
        .map(|v| {
            v.into_iter()
                .map(|r| ResolvedExtraFile {
                    path: r.path,
                    name_template: r.name_template,
                })
                .collect()
        })
        .with_context(|| "checksum: resolve extra_files")
}

// ---------------------------------------------------------------------------
// ChecksumStage
// ---------------------------------------------------------------------------

/// Checksum stage: computes checksums for all build/archive artifacts.
///
/// **Note on `Artifacts` template variable**: This stage does NOT call
/// `ctx.refresh_artifacts_var()` because it only renders naming templates
/// (e.g. `name_template`, `extra_name_template`) — not user-facing release
/// body or announce templates where `{{ Artifacts }}` would be iterated.
/// The `Artifacts` variable is refreshed by the release and announce stages
/// just before they render their body templates.
pub struct ChecksumStage;

impl Stage for ChecksumStage {
    fn name(&self) -> &str {
        "checksum"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("checksum");
        let dry_run = ctx.is_dry_run();

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        let globals = match load_global_defaults(ctx, &log)? {
            Some(g) => g,
            None => return Ok(()),
        };

        validate_all_algorithms(&globals.algorithm, &ctx.config.crate_universe())?;

        // Collect crate configs up-front to avoid borrow conflicts.
        let crates: Vec<_> = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        // Track extra-file paths already checksummed so a workspace run with
        // `defaults.checksum.extra_files` doesn't add the same line N times
        // across N per-crate combined files.
        let mut seen_extra_paths: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();

        for crate_cfg in &crates {
            let crate_name = &crate_cfg.name;

            let crate_skip = crate_cfg.checksum.as_ref().and_then(|c| c.skip.clone());
            if ctx.skip_with_log(
                &crate_skip,
                &log,
                &format!("checksum for crate {crate_name}"),
            )? {
                continue;
            }

            let resolved = resolve_per_crate_config(crate_cfg, &globals);

            let source_artifacts = collect_source_artifacts(
                ctx,
                &log,
                crate_cfg,
                &resolved,
                &dist,
                &mut seen_extra_paths,
            )?;

            if source_artifacts.is_empty() {
                log.verbose(&format!(
                    "skipped checksums for crate {crate_name} — no checksummable artifacts"
                ));
                continue;
            }

            validate_split_name_template(&resolved, source_artifacts.len(), crate_name)?;

            let (combined_lines, mut sidecar_artifacts, artifact_checksums) =
                hash_and_emit_sidecars(ctx, &log, &source_artifacts, &resolved, &dist, dry_run)?;
            new_artifacts.append(&mut sidecar_artifacts);

            if !resolved.split {
                let combined_artifact = write_combined_file(
                    ctx,
                    &log,
                    &resolved,
                    &dist,
                    crate_name,
                    combined_lines,
                    dry_run,
                )?;
                new_artifacts.push(combined_artifact);
            } else {
                log.skip_line(
                    ctx.options.show_skipped,
                    &format!("skipped combined checksums file for crate {crate_name} — split mode"),
                );
            }

            propagate_checksum_metadata(ctx, &artifact_checksums);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Run helpers
// ---------------------------------------------------------------------------

/// Pull global checksum defaults from `defaults.checksum`, applying the
/// global `skip:` gate. Returns `None` when the stage is globally skipped
/// (caller should early-return Ok).
fn load_global_defaults(
    ctx: &mut Context,
    log: &StageLogger,
) -> Result<Option<GlobalChecksumDefaults>> {
    let global_skip = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.checksum.as_ref())
        .and_then(|c| c.skip.clone());
    if ctx.skip_with_log(&global_skip, log, "checksum globally")? {
        return Ok(None);
    }

    let global_cksum = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.checksum.as_ref());

    // Defaults policy: route every default through the resolved_*() accessors
    // on ChecksumConfig so the "what's the default?" answer lives in exactly
    // one place (see lazy-vs-eager defaults policy).
    Ok(Some(GlobalChecksumDefaults {
        algorithm: global_cksum
            .map(|c| c.resolved_algorithm().to_string())
            .unwrap_or_else(|| ChecksumConfig::DEFAULT_ALGORITHM.to_string()),
        name_template: global_cksum.and_then(|c| c.name_template.clone()),
        extra_files: global_cksum.and_then(|c| c.extra_files.clone()),
        templated_extra_files: global_cksum.and_then(|c| c.templated_extra_files.clone()),
        ids: global_cksum.and_then(|c| c.ids.clone()),
        split: global_cksum.and_then(|c| c.split),
        split_format: global_cksum.and_then(|c| c.split_format),
    }))
}

/// Validate the global algorithm and every per-crate override.
/// Fails fast so a typo (`algorithm: sha257`) surfaces before build/archive
/// run, not at the first `hash_file` call.
fn validate_all_algorithms(global_algorithm: &str, crates: &[&CrateConfig]) -> Result<()> {
    validate_algorithm(global_algorithm)?;
    for crate_cfg in crates {
        if let Some(alg) = crate_cfg
            .checksum
            .as_ref()
            .and_then(|c| c.algorithm.as_deref())
        {
            validate_algorithm(alg)
                .with_context(|| format!("checksum: crate '{}'", crate_cfg.name))?;
        }
    }
    Ok(())
}

/// Merge per-crate checksum overrides with global defaults.
fn resolve_per_crate_config(
    crate_cfg: &CrateConfig,
    globals: &GlobalChecksumDefaults,
) -> ResolvedChecksumConfig {
    let crate_cksum = crate_cfg.checksum.as_ref();
    ResolvedChecksumConfig {
        algorithm: crate_cksum
            .and_then(|c| c.algorithm.clone())
            .unwrap_or_else(|| globals.algorithm.clone()),
        name_template: crate_cksum
            .and_then(|c| c.name_template.clone())
            .or_else(|| globals.name_template.clone()),
        extra_files: crate_cksum
            .and_then(|c| c.extra_files.clone())
            .or_else(|| globals.extra_files.clone()),
        templated_extra_files: crate_cksum
            .and_then(|c| c.templated_extra_files.clone())
            .or_else(|| globals.templated_extra_files.clone()),
        ids_filter: crate_cksum
            .and_then(|c| c.ids.clone())
            .or_else(|| globals.ids.clone()),
        split: crate_cksum
            .and_then(|c| c.split)
            .or(globals.split)
            .unwrap_or(false),
        split_format: crate_cksum
            .and_then(|c| c.split_format)
            .or(globals.split_format)
            .unwrap_or_default(),
    }
}

/// Gather checksummable artifacts for one crate: every registered PRIMARY
/// subject artifact, filtered by `ids`, plus synthetic entries derived from
/// `extra_files` and `templated_extra_files`. Source-of-truth for "what gets
/// hashed" is `checksummable_subject_kinds()` — the primary subject taxonomy
/// that EXCLUDES every derived sidecar (Checksum/Signature/Certificate/
/// Metadata). Driving from the subject set (not `release_uploadable_kinds()`,
/// which legitimately contains those sidecars as upload targets) makes a
/// checksum-of-a-signature (`X.sig.sha256`) — and thus the recursive
/// sha256.sig.sha256 chain — unrepresentable by construction.
fn collect_source_artifacts(
    ctx: &mut Context,
    log: &StageLogger,
    crate_cfg: &CrateConfig,
    resolved: &ResolvedChecksumConfig,
    dist: &Path,
    seen_extra_paths: &mut std::collections::HashSet<PathBuf>,
) -> Result<Vec<Artifact>> {
    let crate_name = &crate_cfg.name;
    let mut source_artifacts: Vec<Artifact> = Vec::new();

    for kind in anodizer_core::artifact::checksummable_subject_kinds()
        .iter()
        .copied()
    {
        let artifacts = ctx
            .artifacts
            .by_kind_and_crate(kind, crate_name)
            .into_iter()
            // A directory bundle (the macOS `.app`) shares ArtifactKind::Installer
            // with `.msi`/`.exe` but cannot be hashed as a file; the `.dmg`/`.pkg`
            // wrapping it remain checksum subjects.
            .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
            .cloned();
        if resolved.ids_filter.is_some() {
            source_artifacts
                .extend(artifacts.filter(|a| matches_id_filter(a, resolved.ids_filter.as_deref())));
        } else {
            source_artifacts.extend(artifacts);
        }
    }

    if let Some(ref specs) = resolved.extra_files {
        let resolved_efs = resolve_extra_files(specs, log)?;
        for ef in resolved_efs {
            if !seen_extra_paths.insert(ef.path.clone()) {
                continue;
            }
            let mut metadata = HashMap::from([("extra_file".to_string(), "true".to_string())]);
            if let Some(tmpl) = ef.name_template {
                metadata.insert("extra_name_template".to_string(), tmpl);
            }
            let name = ef
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            source_artifacts.push(Artifact {
                // Synthetic extra-file checksum sources are not archive
                // artifacts; tag them as UploadableFile so downstream stages
                // don't accidentally treat them like build outputs.
                kind: ArtifactKind::UploadableFile,
                name,
                path: ef.path,
                target: None,
                crate_name: crate_name.clone(),
                metadata,
                size: None,
            });
        }
    }

    if let Some(ref tpl_specs) = resolved.templated_extra_files
        && !tpl_specs.is_empty()
    {
        let rendered = anodizer_core::templated_files::process_templated_extra_files(
            tpl_specs, ctx, dist, "checksum",
        )?;
        for (path, dst_name) in rendered {
            if !seen_extra_paths.insert(path.clone()) {
                continue;
            }
            let metadata = HashMap::from([("extra_file".to_string(), "true".to_string())]);
            source_artifacts.push(Artifact {
                kind: ArtifactKind::UploadableFile,
                name: dst_name,
                path,
                target: None,
                crate_name: crate_name.clone(),
                metadata,
                size: None,
            });
        }
    }

    Ok(source_artifacts)
}

/// Split mode writes one sidecar per source artifact. If the user supplies a
/// name_template that doesn't reference `ArtifactName` (or another
/// per-artifact variable) every sidecar resolves to the same path and the
/// last write silently overwrites the rest. Refuse upfront.
fn validate_split_name_template(
    resolved: &ResolvedChecksumConfig,
    source_count: usize,
    crate_name: &str,
) -> Result<()> {
    if resolved.split
        && let Some(tmpl) = &resolved.name_template
        && !tmpl.contains("ArtifactName")
        && source_count > 1
    {
        bail!(
            "checksum: split mode requires the name_template to reference \
             {{{{ ArtifactName }}}} (or another per-artifact variable) so each \
             sidecar writes to a distinct path; got name_template = {:?} for \
             crate '{}'",
            tmpl,
            crate_name
        );
    }
    Ok(())
}

/// Hash one artifact, returning a zero-filled placeholder of the correct
/// length for non-existent dry-run paths.
fn compute_artifact_hash(
    artifact: &Artifact,
    algorithm: &str,
    crate_name: &str,
    dry_run: bool,
    log: &StageLogger,
) -> Result<String> {
    if dry_run && !artifact.path.exists() {
        log.verbose(&format!(
            "(dry-run) skipped hash — non-existent {}",
            artifact.path.display()
        ));
        let hash_len = hash_hex_len(algorithm);
        Ok("0".repeat(hash_len))
    } else {
        hash_file(&artifact.path, algorithm).with_context(|| {
            format!(
                "checksum: hashing {} for crate {crate_name}",
                artifact.path.display()
            )
        })
    }
}

/// Render the display name for an artifact in a combined-file line.
/// `extra_name_template` lets users alias an extra file under a different
/// name; default falls back to the raw filename.
fn resolve_checksum_display_name(
    ctx: &Context,
    artifact: &Artifact,
    filename: &str,
    artifact_ext: &str,
    algorithm: &str,
) -> Result<String> {
    if let Some(tmpl) = artifact.metadata.get("extra_name_template") {
        let mut vars = ctx.template_vars().clone();
        vars.set("ArtifactName", filename);
        vars.set("ArtifactExt", artifact_ext);
        // `Algorithm` parity with the sidecar name_template path — users
        // writing `{{ .ArtifactName }}.{{ .Algorithm }}` in extra_files
        // name_template expect it available here too.
        vars.set("Algorithm", algorithm);
        anodizer_core::template::render(tmpl, &vars).with_context(|| {
            format!("checksum: render extra_name_template '{tmpl}' for {filename}")
        })
    } else {
        Ok(filename.to_string())
    }
}

/// Compute the on-disk sidecar path for split mode.
/// `<artifact>.<algorithm>` when no name_template is set; otherwise renders
/// the template with `ArtifactName`/`ArtifactExt`/`Algorithm` vars.
/// Sidecars are placed in dist.
fn resolve_sidecar_path(
    ctx: &Context,
    artifact: &Artifact,
    name_template: Option<&str>,
    filename: &str,
    artifact_ext: &str,
    algorithm: &str,
    dist: &Path,
) -> Result<PathBuf> {
    if let Some(tmpl) = name_template {
        let mut vars = ctx.template_vars().clone();
        vars.set("ArtifactName", filename);
        vars.set("ArtifactExt", artifact_ext);
        vars.set("Algorithm", algorithm);
        let rendered = anodizer_core::template::render(tmpl, &vars).with_context(|| {
            format!(
                "checksum: render split name_template for {}",
                artifact.path.display()
            )
        })?;
        Ok(dist.join(rendered))
    } else {
        Ok(dist.join(format!("{}.{}", filename, algorithm)))
    }
}

/// Hash every source artifact, build combined-file lines, and (in split
/// mode) emit one sidecar per artifact. Returns
/// `(combined_lines, sidecar_artifacts, artifact_checksums)`.
/// `artifact_checksums` is `(path, algo, hex_hash)` used downstream by
/// `propagate_checksum_metadata`.
#[allow(clippy::type_complexity)]
fn hash_and_emit_sidecars(
    ctx: &mut Context,
    log: &StageLogger,
    source_artifacts: &[Artifact],
    resolved: &ResolvedChecksumConfig,
    dist: &Path,
    dry_run: bool,
) -> Result<(Vec<String>, Vec<Artifact>, Vec<(PathBuf, String, String)>)> {
    let mut combined_lines: Vec<String> = Vec::new();
    let mut new_sidecars: Vec<Artifact> = Vec::new();
    // (artifact_path, algorithm, hex-hash). algorithm + bare hash kept
    // separate so callers can write both `Checksum = algo:hash` (legacy) and
    // `<algo> = hash` (publisher-friendly, the per-artifact
    // metadata convention).
    let mut artifact_checksums: Vec<(PathBuf, String, String)> = Vec::new();

    for artifact in source_artifacts {
        let hash = compute_artifact_hash(
            artifact,
            &resolved.algorithm,
            &artifact.crate_name,
            dry_run,
            log,
        )?;

        artifact_checksums.push((
            artifact.path.clone(),
            resolved.algorithm.clone(),
            hash.clone(),
        ));

        // Non-UTF8 filenames produce a lossy display rather than a sentinel
        // string. Any filename anodizer wrote is UTF-8; this only triggers
        // for user-supplied paths. Lossy gives the user something searchable
        // instead of collapsing every problematic name to one bucket.
        let filename_owned = artifact
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| artifact.path.display().to_string());
        let filename = filename_owned.as_str();

        let artifact_ext = artifact.ext();
        let artifact_ext = artifact_ext.as_str();
        let checksum_name = resolve_checksum_display_name(
            ctx,
            artifact,
            filename,
            artifact_ext,
            &resolved.algorithm,
        )?;

        combined_lines.push(format_checksum_line(&hash, &checksum_name));

        if resolved.split {
            let sidecar_path = resolve_sidecar_path(
                ctx,
                artifact,
                resolved.name_template.as_deref(),
                filename,
                artifact_ext,
                &resolved.algorithm,
                dist,
            )?;

            // `bare` writes only the raw hex hash (no filename, no trailing
            // newline) for GoReleaser parity; `coreutils` writes
            // `<hash>  <filename>\n` so the sidecar verifies with `shasum -c`.
            if !dry_run {
                let mut sidecar_file = File::create(&sidecar_path).with_context(|| {
                    format!("checksum: create sidecar {}", sidecar_path.display())
                })?;
                match resolved.split_format {
                    ChecksumSplitFormat::Bare => write!(sidecar_file, "{}", hash),
                    ChecksumSplitFormat::Coreutils => {
                        writeln!(
                            sidecar_file,
                            "{}",
                            format_checksum_line(&hash, &checksum_name)
                        )
                    }
                }
                .with_context(|| format!("checksum: write sidecar {}", sidecar_path.display()))?;
            }

            log.verbose(&format!(
                "{}{} → {} ({})",
                if dry_run { "(dry-run) " } else { "" },
                artifact.path.display(),
                sidecar_path.display(),
                resolved.algorithm
            ));

            let sidecar_name = sidecar_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| sidecar_path.display().to_string());
            new_sidecars.push(Artifact {
                kind: ArtifactKind::Checksum,
                name: sidecar_name,
                path: sidecar_path,
                target: artifact.target.clone(),
                crate_name: artifact.crate_name.clone(),
                metadata: HashMap::from([
                    ("algorithm".to_string(), resolved.algorithm.clone()),
                    // The checksum-of extra — the path of the
                    // artifact this checksum is for.
                    (
                        "ChecksumOf".to_string(),
                        artifact.path.to_string_lossy().into_owned(),
                    ),
                ]),
                size: None,
            });
        }
    }

    // Sort combined lines by filename for deterministic output / reproducible
    // builds.
    //
    // Edge case (using
    // `strings.Split(a, "  ")[1]`): filenames containing a two-space sequence
    // will be sorted by the prefix before the *first* double-space, producing
    // a wrong sort key. Intentional —
    // `test_combined_sort_doublespace_divergence` will flag a change. In
    // practice artifact filenames never contain double-spaces.
    combined_lines.sort_by(|a, b| {
        let name_a = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
        let name_b = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
        name_a.cmp(name_b)
    });

    Ok((combined_lines, new_sidecars, artifact_checksums))
}

/// Materialize the combined checksums file (non-split mode), set the
/// `Checksums` template variable for release-body templates, and return the
/// registered artifact.
#[allow(clippy::too_many_arguments)]
fn write_combined_file(
    ctx: &mut Context,
    log: &StageLogger,
    resolved: &ResolvedChecksumConfig,
    dist: &Path,
    crate_name: &str,
    combined_lines: Vec<String>,
    dry_run: bool,
) -> Result<Artifact> {
    // Default routed through `ChecksumConfig::DEFAULT_NAME_TEMPLATE` so the
    // Canonical fallback lives next to its sibling resolved_*() accessors
    // instead of in a stage-local literal.
    let tmpl_str: &str = resolved
        .name_template
        .as_deref()
        .unwrap_or(ChecksumConfig::DEFAULT_NAME_TEMPLATE);
    let combined_filename = ctx
        .render_template(tmpl_str)
        .with_context(|| format!("checksum: render name_template for {crate_name}"))?;

    let combined_path = dist.join(&combined_filename);

    // Each line gets "\n" appended, then all are joined
    // with no separator (strings.Join(lines, "")).
    let content: String = combined_lines.iter().map(|l| format!("{}\n", l)).collect();

    // Set the Checksums template variable so release body templates can
    // reference {{ .Checksums }}.
    ctx.template_vars_mut().set("Checksums", &content);

    // Only write files in non-dry-run mode; hash computation and artifact
    // registration always happen so downstream stages (sign, release) can
    // reference checksums.
    if !dry_run {
        std::fs::create_dir_all(dist)
            .with_context(|| format!("checksum: create dist dir {}", dist.display()))?;
        let mut combined_file = File::create(&combined_path).with_context(|| {
            format!("checksum: create combined file {}", combined_path.display())
        })?;
        write!(combined_file, "{}", content).with_context(|| {
            format!("checksum: write combined file {}", combined_path.display())
        })?;
    }

    log.status(&format!(
        "{}combined checksums → {}",
        if dry_run { "(dry-run) " } else { "" },
        combined_path.display()
    ));

    let combined_name = combined_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| combined_path.display().to_string());
    Ok(Artifact {
        kind: ArtifactKind::Checksum,
        name: combined_name,
        path: combined_path,
        target: None,
        crate_name: crate_name.to_string(),
        metadata: HashMap::from([
            ("algorithm".to_string(), resolved.algorithm.clone()),
            (
                anodizer_core::artifact::COMBINED_CHECKSUM_META.to_string(),
                anodizer_core::artifact::COMBINED_CHECKSUM_VALUE.to_string(),
            ),
        ]),
        size: None,
    })
}

/// Write the per-source computed hash back into the source artifact's
/// metadata under TWO keys:
///   `Checksum = <algo>:<hash>`  (legacy, kept for back-compat)
///   `<algo>   = <hash>`         (lowercase — what every publisher
///                                (winget, krew, homebrew, scoop,
///                                chocolatey) reads when building manifests).
fn propagate_checksum_metadata(
    ctx: &mut Context,
    artifact_checksums: &[(PathBuf, String, String)],
) {
    let checksum_map: std::collections::HashMap<&PathBuf, (&String, &String)> = artifact_checksums
        .iter()
        .map(|(p, a, h)| (p, (a, h)))
        .collect();
    for art in ctx.artifacts.all_mut() {
        if let Some((algo, hash)) = checksum_map.get(&art.path) {
            art.metadata
                .entry("Checksum".to_string())
                .or_insert_with(|| format!("{}:{}", algo, hash));
            art.metadata
                .entry((*algo).clone())
                .or_insert_with(|| (*hash).clone());
        }
    }
}

// ---------------------------------------------------------------------------
// refresh_combined_checksums — recompute and rewrite combined checksum files
// ---------------------------------------------------------------------------

/// Compile-time coupling to the determinism harness's aggregate registry: the
/// combined checksums file this module writes is recognized by
/// `anodizer_core::determinism::CombinedChecksums`, whose `id()` is this const.
/// Referencing it here means renaming/removing the id breaks this build, so the
/// producer and the registry entry cannot silently drift apart (mirrors the
/// `MSVC_DETERMINISM_RUSTFLAGS` ↔ `.cargo/config.toml` coupling in core).
const _: &str = anodizer_core::determinism::COMBINED_CHECKSUMS_AGGREGATE_ID;

/// Refresh any combined `checksums.txt` files in-place by recomputing hashes
/// of all non-checksum, non-signature, non-certificate artifacts currently in
/// the registry. After signing (which produces new signature artifacts),
/// the checksum file is regenerated so signed artifacts that happen to be
/// uploadable appear in the final sums.
///
/// Only combined checksum artifacts (metadata key `combined = "true"`) are
/// rewritten — split sidecars are per-artifact and never need refresh.
pub fn refresh_combined_checksums(ctx: &mut Context, dry_run: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }

    // Collect combined checksum artifacts (per crate). The selection
    // predicate is the shared core definition so the verify-release gate's
    // cross-leg exemption stays welded to exactly the set rewritten here.
    let combined: Vec<(PathBuf, String, String)> = ctx
        .artifacts
        .by_kind(ArtifactKind::Checksum)
        .into_iter()
        .filter(|a| anodizer_core::artifact::is_combined_checksum_artifact(a))
        .filter_map(|a| {
            let algo = a.metadata.get("algorithm")?.clone();
            Some((a.path.clone(), algo, a.crate_name.clone()))
        })
        .collect();

    if combined.is_empty() {
        return Ok(());
    }

    for (checksum_path, algorithm, crate_name) in combined {
        let mut lines: Vec<String> = Vec::new();
        for artifact in ctx.artifacts.all() {
            if artifact.crate_name != crate_name {
                continue;
            }
            // Derived sidecars (Checksum/Signature/Certificate/Metadata) are
            // the output of checksumming/signing, never a subject — re-hashing
            // a .sig here is what produced the sha256.sig.sha256 chains.
            if anodizer_core::artifact::is_derived_sidecar_kind(artifact.kind) {
                continue;
            }
            // The macOS `.app` directory bundle is never a hash subject.
            if anodizer_core::artifact::is_directory_bundle_artifact(artifact) {
                continue;
            }
            if !artifact.path.exists() {
                continue;
            }
            let hash = hash_file(&artifact.path, &algorithm)?;
            let fname = artifact
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            lines.push(format_checksum_line(&hash, &fname));
        }

        // Deterministic order (match the original combined-writer).
        lines.sort_by(|a, b| {
            let na = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
            let nb = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
            na.cmp(nb)
        });

        let content: String = lines.iter().map(|l| format!("{l}\n")).collect();
        if let Some(parent) = checksum_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("refresh checksum: create parent {}", parent.display()))?;
        }
        let mut f = File::create(&checksum_path)
            .with_context(|| format!("refresh checksum: create {}", checksum_path.display()))?;
        write!(f, "{content}")
            .with_context(|| format!("refresh checksum: write {}", checksum_path.display()))?;
    }

    Ok(())
}
