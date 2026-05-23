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
//! uploadable land in the final sums (matches GoReleaser's `ExtraRefresh`).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::{ChecksumConfig, ExtraFileSpec};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use super::hashing::{format_checksum_line, hash_file, hash_hex_len, validate_algorithm};

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

        // Extract global checksum defaults once
        let global_cksum = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref());

        let global_skip = global_cksum.and_then(|c| c.skip.clone());
        if ctx.skip_with_log(&global_skip, &log, "checksum globally")? {
            return Ok(());
        }

        // Defaults policy: route every default through the resolved_*()
        // accessors on ChecksumConfig so the "what's the default?" answer
        // lives in exactly one place (see lazy-vs-eager defaults policy).
        let global_algorithm = global_cksum
            .map(|c| c.resolved_algorithm().to_string())
            .unwrap_or_else(|| ChecksumConfig::DEFAULT_ALGORITHM.to_string());
        let global_name_template = global_cksum.and_then(|c| c.name_template.clone());
        let global_extra_files = global_cksum.and_then(|c| c.extra_files.clone());
        let global_templated_extra_files =
            global_cksum.and_then(|c| c.templated_extra_files.clone());
        let global_ids = global_cksum.and_then(|c| c.ids.clone());
        let global_split = global_cksum.and_then(|c| c.split);

        // Fail fast on unsupported algorithm names — global default first, then
        // every per-crate override. Without this, a typo (`algorithm: sha257`)
        // only surfaces at first hash_file call, which is after build/archive
        // have already run.
        validate_algorithm(&global_algorithm)?;
        for crate_cfg in &ctx.config.crates {
            if let Some(alg) = crate_cfg
                .checksum
                .as_ref()
                .and_then(|c| c.algorithm.as_deref())
            {
                validate_algorithm(alg)
                    .with_context(|| format!("checksum: crate '{}'", crate_cfg.name))?;
            }
        }

        // Collect crate configs up-front to avoid borrow conflicts
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
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

            // Skip crates that have checksum explicitly set to skip
            let crate_skip = crate_cfg.checksum.as_ref().and_then(|c| c.skip.clone());
            if ctx.skip_with_log(
                &crate_skip,
                &log,
                &format!("checksum for crate {crate_name}"),
            )? {
                continue;
            }

            // Per-crate overrides (fall back to global defaults)
            let crate_cksum = crate_cfg.checksum.as_ref();
            let algorithm = crate_cksum
                .and_then(|c| c.algorithm.clone())
                .unwrap_or_else(|| global_algorithm.clone());
            let name_template = crate_cksum
                .and_then(|c| c.name_template.clone())
                .or_else(|| global_name_template.clone());
            let extra_files = crate_cksum
                .and_then(|c| c.extra_files.clone())
                .or_else(|| global_extra_files.clone());
            let templated_extra_files = crate_cksum
                .and_then(|c| c.templated_extra_files.clone())
                .or_else(|| global_templated_extra_files.clone());
            let ids_filter = crate_cksum
                .and_then(|c| c.ids.clone())
                .or_else(|| global_ids.clone());
            let split = crate_cksum
                .and_then(|c| c.split)
                .or(global_split)
                .unwrap_or(false);

            // Gather checksummable artifacts for this crate. Source-of-truth is
            // `release_uploadable_kinds()` minus `Checksum` itself — mirroring
            // GoReleaser's `Not(ByType(Checksum))` filter in
            // `internal/pipe/checksums/checksums.go::buildArtifactList`. Cross-
            // linking here means stage-checksum, stage-release upload, and the
            // stage-sign "all" filter all reason about the same artifact set.
            let mut source_artifacts: Vec<Artifact> = Vec::new();
            for kind in anodizer_core::artifact::release_uploadable_kinds()
                .iter()
                .copied()
                .filter(|k| *k != ArtifactKind::Checksum)
            {
                let artifacts = ctx
                    .artifacts
                    .by_kind_and_crate(kind, crate_name)
                    .into_iter()
                    .cloned();
                if ids_filter.is_some() {
                    source_artifacts
                        .extend(artifacts.filter(|a| matches_id_filter(a, ids_filter.as_deref())));
                } else {
                    source_artifacts.extend(artifacts);
                }
            }

            if let Some(ref specs) = extra_files {
                let resolved = resolve_extra_files(specs, &log)?;
                for ef in resolved {
                    if !seen_extra_paths.insert(ef.path.clone()) {
                        continue;
                    }
                    let mut metadata =
                        HashMap::from([("extra_file".to_string(), "true".to_string())]);
                    if let Some(tmpl) = ef.name_template {
                        metadata.insert("extra_name_template".to_string(), tmpl);
                    }
                    let name = ef
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    source_artifacts.push(Artifact {
                        // Synthetic extra-file checksum sources are not
                        // archive artifacts; tag them as UploadableFile so
                        // downstream stages don't accidentally treat them
                        // like build outputs.
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

            if let Some(ref tpl_specs) = templated_extra_files
                && !tpl_specs.is_empty()
            {
                let rendered = anodizer_core::templated_files::process_templated_extra_files(
                    tpl_specs, ctx, &dist, "checksum",
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

            if source_artifacts.is_empty() {
                log.verbose(&format!(
                    "no checksummable artifacts for crate {crate_name}, skipping"
                ));
                continue;
            }

            // Extension for individual sidecar files
            let ext = &algorithm; // e.g. "sha256" or "sha512"

            let mut combined_lines: Vec<String> = Vec::new();
            // Collect (artifact_path, "algorithm:hash") pairs so we can store
            // the checksum back into each artifact's metadata after the loop.
            // GoReleaser stores this as Extra["Checksum"] = "algorithm:hash".
            // (artifact_path, algorithm, hex-hash) — algorithm + bare hash kept
            // separate so we can write both "Checksum" = "algo:hash" (legacy)
            // and "<algo>" = hash (publisher-friendly, matches GoReleaser's
            // per-artifact metadata convention).
            let mut artifact_checksums: Vec<(PathBuf, String, String)> = Vec::new();

            // Split mode writes one sidecar per source artifact. If the user
            // supplies a name_template that doesn't reference ArtifactName
            // (or Algorithm) every sidecar resolves to the same path and the
            // last write silently overwrites the rest. Refuse upfront with a
            // pointer at what to add.
            if split
                && let Some(tmpl) = &name_template
                && !tmpl.contains("ArtifactName")
                && source_artifacts.len() > 1
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

            for artifact in &source_artifacts {
                // In dry-run mode, files may not exist on disk; skip with placeholder
                let hash = if dry_run && !artifact.path.exists() {
                    log.verbose(&format!(
                        "(dry-run) skipping hash for non-existent {}",
                        artifact.path.display()
                    ));
                    // Produce a placeholder hash with the correct length for the algorithm
                    let hash_len = hash_hex_len(&algorithm);
                    "0".repeat(hash_len)
                } else {
                    hash_file(&artifact.path, &algorithm).with_context(|| {
                        format!(
                            "checksum: hashing {} for crate {crate_name}",
                            artifact.path.display()
                        )
                    })?
                };

                // Store the checksum for later propagation to artifact metadata.
                artifact_checksums.push((artifact.path.clone(), algorithm.clone(), hash.clone()));

                // Non-UTF8 filenames produce a lossy display rather than the
                // sentinel string `"unknown"` — any filename anodizer wrote
                // is UTF-8, so this only triggers for paths handed in by the
                // user. Lossy gives the user something searchable instead
                // of collapsing every problematic name to one bucket.
                let filename_owned = artifact
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| artifact.path.display().to_string());
                let filename = filename_owned.as_str();

                // Determine the display name for this artifact in the checksum line.
                // If the extra file has a name_template, render it to get an alias.
                let artifact_ext = artifact.ext();
                let artifact_ext = artifact_ext.as_str();
                let checksum_name = if let Some(tmpl) = artifact.metadata.get("extra_name_template")
                {
                    let mut vars = ctx.template_vars().clone();
                    vars.set("ArtifactName", filename);
                    vars.set("ArtifactExt", artifact_ext);
                    // `Algorithm` parity with the sidecar name_template path
                    // below — users writing `{{ .ArtifactName }}.{{ .Algorithm
                    // }}` in extra_files name_template expect it available
                    // here too.
                    vars.set("Algorithm", &algorithm);
                    anodizer_core::template::render(tmpl, &vars).with_context(|| {
                        format!("checksum: render extra_name_template '{tmpl}' for {filename}")
                    })?
                } else {
                    filename.to_string()
                };

                let line = format_checksum_line(&hash, &checksum_name);
                combined_lines.push(line);

                // Only create sidecar files in split mode
                if split {
                    let sidecar_path = if let Some(tmpl) = &name_template {
                        // Use name_template for sidecar naming when provided
                        let mut vars = ctx.template_vars().clone();
                        vars.set("ArtifactName", filename);
                        vars.set("ArtifactExt", artifact_ext);
                        vars.set("Algorithm", &algorithm);
                        let rendered =
                            anodizer_core::template::render(tmpl, &vars).with_context(|| {
                                format!(
                                    "checksum: render split name_template for {}",
                                    artifact.path.display()
                                )
                            })?;
                        // GoReleaser places sidecars in dist (checksums.go:79)
                        Path::new(&dist).join(rendered)
                    } else {
                        // Default sidecar naming: {artifact}.{algorithm}
                        // GoReleaser places sidecars in dist (checksums.go:79)
                        Path::new(&dist).join(format!("{}.{}", filename, ext))
                    };

                    // GoReleaser writes ONLY the raw hex hash in sidecar files
                    // (no filename, no trailing newline).
                    if !dry_run {
                        let mut sidecar_file = File::create(&sidecar_path).with_context(|| {
                            format!("checksum: create sidecar {}", sidecar_path.display())
                        })?;
                        write!(sidecar_file, "{}", hash).with_context(|| {
                            format!("checksum: write sidecar {}", sidecar_path.display())
                        })?;
                    }

                    log.verbose(&format!(
                        "{}{} -> {} ({})",
                        if dry_run { "(dry-run) " } else { "" },
                        artifact.path.display(),
                        sidecar_path.display(),
                        algorithm
                    ));

                    // Register sidecar as a Checksum artifact
                    let sidecar_name = sidecar_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| sidecar_path.display().to_string());
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Checksum,
                        name: sidecar_name,
                        path: sidecar_path,
                        target: artifact.target.clone(),
                        crate_name: crate_name.clone(),
                        metadata: HashMap::from([
                            ("algorithm".to_string(), algorithm.clone()),
                            // GoReleaser artifact.ExtraChecksumOf — the path
                            // of the artifact this checksum is for.
                            (
                                "ChecksumOf".to_string(),
                                artifact.path.to_string_lossy().into_owned(),
                            ),
                        ]),
                        size: None,
                    });
                }
            }

            // Sort combined lines by filename (the part after "  ") for
            // deterministic output and reproducible builds.
            //
            // Edge case — inherited from GoReleaser (checksums.go:171-174
            // uses `strings.Split(a, "  ")[1]`): filenames that themselves
            // contain a two-space sequence will be sorted by the prefix
            // before the *first* double-space, producing a wrong sort key.
            // This is intentionally matched to GoReleaser behavior; changing
            // it would diverge and the divergence test
            // `test_combined_sort_doublespace_divergence` will flag a fix.
            // In practice artifact filenames never contain double-spaces so
            // this is benign — but documented so future refactors don't
            // silently "improve" it.
            combined_lines.sort_by(|a, b| {
                let name_a = a.split_once("  ").map(|(_, n)| n).unwrap_or(a);
                let name_b = b.split_once("  ").map(|(_, n)| n).unwrap_or(b);
                name_a.cmp(name_b)
            });

            // Write combined checksums file (only when NOT in split mode).
            // Route the default through `ChecksumConfig::DEFAULT_NAME_TEMPLATE`
            // so the GR-canonical fallback lives next to its sibling
            // resolved_*() accessors instead of in a stage-local literal.
            if !split {
                let tmpl_str: &str = name_template
                    .as_deref()
                    .unwrap_or(ChecksumConfig::DEFAULT_NAME_TEMPLATE);
                let combined_filename = ctx
                    .render_template(tmpl_str)
                    .with_context(|| format!("checksum: render name_template for {crate_name}"))?;

                let combined_path = dist.join(&combined_filename);

                // Build the combined content string for both file writing and
                // the Checksums template variable.
                // Match GoReleaser: each line gets "\n" appended, then
                // all are joined with no separator (strings.Join(lines, "")).
                let content: String = combined_lines.iter().map(|l| format!("{}\n", l)).collect();

                // Set the Checksums template variable so release body templates
                // can reference {{ .Checksums }}.
                ctx.template_vars_mut().set("Checksums", &content);

                // Only write files in non-dry-run mode; hash computation and
                // artifact registration always happen so downstream stages
                // (sign, release) can reference checksums.
                if !dry_run {
                    std::fs::create_dir_all(&dist)
                        .with_context(|| format!("checksum: create dist dir {}", dist.display()))?;

                    let mut combined_file = File::create(&combined_path).with_context(|| {
                        format!("checksum: create combined file {}", combined_path.display())
                    })?;
                    write!(combined_file, "{}", content).with_context(|| {
                        format!("checksum: write combined file {}", combined_path.display())
                    })?;
                }

                log.status(&format!(
                    "{}combined checksums -> {}",
                    if dry_run { "(dry-run) " } else { "" },
                    combined_path.display()
                ));

                let combined_name = combined_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| combined_path.display().to_string());
                new_artifacts.push(Artifact {
                    kind: ArtifactKind::Checksum,
                    name: combined_name,
                    path: combined_path,
                    target: None,
                    crate_name: crate_name.clone(),
                    metadata: HashMap::from([
                        ("algorithm".to_string(), algorithm.clone()),
                        ("combined".to_string(), "true".to_string()),
                    ]),
                    size: None,
                });
            } else {
                log.status(&format!(
                    "split mode: skipping combined checksums file for crate {crate_name}"
                ));
            }

            // Propagate the computed checksum back into each source
            // artifact's metadata under TWO keys:
            //   "Checksum" = "<algo>:<hash>"  (legacy, kept for back-compat)
            //   "<algo>"   = "<hash>"          (lowercase, e.g. "sha256" =
            //                                  "<hex>") — what every publisher
            //                                  (winget, krew, homebrew, scoop,
            //                                  chocolatey) actually reads when
            //                                  building manifests.
            let checksum_map: std::collections::HashMap<&PathBuf, (&String, &String)> =
                artifact_checksums
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

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// refresh_combined_checksums — recompute and rewrite combined checksum files
// ---------------------------------------------------------------------------

/// Refresh any combined `checksums.txt` files in-place by recomputing hashes
/// of all non-checksum, non-signature, non-certificate artifacts currently in
/// the registry. This matches GoReleaser's `ExtraRefresh` closure pattern
/// (release.go:121): after signing (which produces new signature artifacts),
/// the checksum file is regenerated so signed artifacts that happen to be
/// uploadable appear in the final sums.
///
/// Only combined checksum artifacts (metadata key `combined = "true"`) are
/// rewritten — split sidecars are per-artifact and never need refresh.
pub fn refresh_combined_checksums(ctx: &mut Context, dry_run: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }

    // Collect combined checksum artifacts (per crate).
    let combined: Vec<(PathBuf, String, String)> = ctx
        .artifacts
        .by_kind(ArtifactKind::Checksum)
        .into_iter()
        .filter(|a| a.metadata.get("combined").map(|s| s.as_str()) == Some("true"))
        .filter_map(|a| {
            let algo = a.metadata.get("algorithm")?.clone();
            Some((a.path.clone(), algo, a.crate_name.clone()))
        })
        .collect();

    if combined.is_empty() {
        return Ok(());
    }

    for (checksum_path, algorithm, crate_name) in combined {
        // Kinds that are checksummed upstream; Signature/Certificate/Checksum
        // are never hashed (they're the signing/checksum output themselves).
        let skip_kinds = [
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Certificate,
        ];

        let mut lines: Vec<String> = Vec::new();
        for artifact in ctx.artifacts.all() {
            if artifact.crate_name != crate_name {
                continue;
            }
            if skip_kinds.contains(&artifact.kind) {
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
