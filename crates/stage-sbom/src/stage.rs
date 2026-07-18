use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::SbomConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use super::*;

// ---------------------------------------------------------------------------
// SbomStage — independent pipeline stage
// ---------------------------------------------------------------------------

pub struct SbomStage;

impl Stage for SbomStage {
    fn name(&self) -> &str {
        "sbom"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("sbom");

        if ctx.config.sboms.is_empty() {
            log.status("skipped SBOM — none configured");
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        if !ctx.is_dry_run() {
            std::fs::create_dir_all(&dist)
                .with_context(|| format!("sbom: failed to create dist dir: {}", dist.display()))?;
        }

        // Validate ID uniqueness
        let mut seen_ids = std::collections::HashSet::new();
        for cfg in &ctx.config.sboms {
            let id = cfg.resolved_id();
            if !seen_ids.insert(id.to_string()) {
                bail!(
                    "found multiple sboms with the ID '{}', please fix your config",
                    id
                );
            }
        }

        let configs: Vec<SbomConfig> = ctx.config.sboms.clone();
        for sbom_cfg in &configs {
            run_sbom(ctx, &dist, sbom_cfg)?;
        }

        Ok(())
    }
}

/// Run a single SBOM config — external command or built-in mode.
fn run_sbom(ctx: &mut Context, dist: &Path, sbom_cfg: &SbomConfig) -> Result<()> {
    let log = ctx.logger("sbom");
    let project_name = ctx.config.project_name.clone();
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let id = sbom_cfg.resolved_id();

    // Evaluate skip — supports bool or template string. Use
    // try_evaluates_to_true so a malformed skip: template surfaces as Err
    // instead of silently evaluating false.
    if let Some(ref d) = sbom_cfg.skip
        && d.try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| format!("sbom[{}]: evaluate skip expression", id))?
    {
        log.status(&format!(
            "skipped sbom[{}] — `skip` condition evaluated truthy",
            id
        ));
        return Ok(());
    }

    // Determine if this is a built-in (no external command) or subprocess model
    let use_builtin = sbom_cfg.cmd.is_none() && sbom_cfg.args.is_none();

    // When artifacts != "any", multiple SBOM output documents are
    // unsupported in BOTH modes: each document name is rendered per-artifact
    // and would clobber on collision (and built-in mode would silently
    // truncate to documents[0]). Rejected before the mode dispatch so the
    // built-in path cannot silently ignore explicit user config.
    {
        let artifacts_type = sbom_cfg.resolved_artifacts();
        let documents = sbom_cfg.resolved_documents(artifacts_type);
        if artifacts_type != "any" && documents.len() > 1 {
            anyhow::bail!(
                "sbom[{}]: multiple SBOM outputs when artifacts={:?} is unsupported",
                id,
                artifacts_type
            );
        }
    }

    if use_builtin {
        return run_sbom_builtin(ctx, dist, sbom_cfg, &project_name, &version);
    }

    // --- External command (subprocess) model ---
    let cmd = sbom_cfg.resolved_cmd();
    let artifacts_type = sbom_cfg.resolved_artifacts();

    let documents = sbom_cfg.resolved_documents(artifacts_type);

    let args = sbom_cfg.resolved_args(cmd);

    let env_vars: Vec<(String, String)> = match sbom_cfg.env.as_deref() {
        Some(list) => anodizer_core::config::parse_env_entries(list)
            .with_context(|| "sbom: parse env entries")?,
        None => SbomConfig::default_syft_env_for(cmd, artifacts_type),
    };

    // Filter artifacts from the registry based on artifacts type.
    //
    // For `artifacts: binary` we match Binary + UploadableBinary + UniversalBinary
    // and dedup by path, preferring UploadableBinary (binary-like
    // artifact selection).
    // Without this, each per-arch Binary *plus* its UploadableBinary registration
    // would produce its own SBOM at the same path, causing file collisions.
    let matching_artifacts: Vec<SbomSubject> = match artifacts_type {
        "any" => vec![],
        "binary" => {
            let candidates = ctx.artifacts.binary_like_dedup();
            let pre_ids = candidates.len();
            let matched: Vec<SbomSubject> = candidates
                .into_iter()
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| {
                    (
                        a.path.clone(),
                        a.metadata.clone(),
                        a.target.clone(),
                        Some(a.kind),
                    )
                })
                .collect();
            warn_ids_eliminated_all(&log, id, sbom_cfg.ids.as_deref(), pre_ids, matched.len());
            matched
        }
        _ => {
            let kind = typed_artifact_kind(artifacts_type, id)?;

            // A macOS `.app` bundle registers as Installer + format=appbundle but
            // is a DIRECTORY never uploaded raw (its `.dmg`/`.pkg` wrapper ships).
            // Excluding it stops syft generating a stray SBOM for an asset that
            // never reaches a release.
            let pre_ids = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .count();
            let matched: Vec<SbomSubject> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| {
                    (
                        a.path.clone(),
                        a.metadata.clone(),
                        a.target.clone(),
                        Some(a.kind),
                    )
                })
                .collect();
            warn_ids_eliminated_all(&log, id, sbom_cfg.ids.as_deref(), pre_ids, matched.len());

            if matched.is_empty() {
                ctx.strict_guard(
                    &log,
                    &format!(
                        "skipped SBOM generation — no matching '{}' artifacts found (sbom[{}])",
                        artifacts_type, id
                    ),
                )?;
                return Ok(());
            }

            matched
        }
    };

    if ctx.is_dry_run() {
        if artifacts_type == "any" {
            log.status(&format!(
                "(dry-run) would run '{}' for all artifacts (sbom[{}])",
                cmd, id
            ));
        } else {
            for (path, _, _, _) in &matching_artifacts {
                log.status(&format!(
                    "(dry-run) would run '{}' on {} (sbom[{}])",
                    cmd,
                    path.display(),
                    id
                ));
            }
        }
        return Ok(());
    }

    let artifact_list: Vec<SbomSubject> = if artifacts_type == "any" {
        vec![(PathBuf::new(), HashMap::new(), None, None)]
    } else {
        matching_artifacts
    };

    for (artifact_path, artifact_meta, artifact_target, artifact_kind) in &artifact_list {
        let artifact_rel = if artifact_path.as_os_str().is_empty() {
            String::new()
        } else {
            artifact_path
                .strip_prefix(dist)
                .unwrap_or(artifact_path)
                .display()
                .to_string()
        };

        let vars = artifact_template_vars(
            ctx,
            artifact_path,
            artifact_meta,
            artifact_target.as_deref(),
        );

        let mut rendered_docs: Vec<String> = Vec::new();
        for doc_tpl in &documents {
            let rendered = anodizer_core::template::render(doc_tpl, &vars).with_context(|| {
                format!(
                    "sbom[{}]: failed to render document template '{}'",
                    id, doc_tpl
                )
            })?;
            // Document paths are joined onto `dist/` for both write and
            // artifact registration. An absolute path would silently bypass
            // dist (Path::join discards the base when joined with absolute)
            // and produce an artifact registered at a nonexistent
            // dist/$rendered location. Absolute paths are refused
            // here for the same reason — keep SBOMs inside dist or the
            // checksum/release stages can't find them.
            if Path::new(&rendered).is_absolute() {
                bail!(
                    "sbom[{}]: rendered document path '{}' is absolute; \
                     SBOM outputs must be relative to the dist directory",
                    id,
                    rendered
                );
            }
            rendered_docs.push(rendered);
        }

        let first_doc = rendered_docs.first().cloned().unwrap_or_default();

        let artifact_id = artifact_meta.get("id").map(|s| s.as_str()).unwrap_or("");
        let mut rendered_args: Vec<String> = Vec::with_capacity(args.len());
        for arg in &args {
            let mut s = arg.replace("$artifactID", artifact_id);
            s = s.replace("$artifact", &artifact_rel);
            for (i, doc) in rendered_docs.iter().enumerate() {
                s = s.replace(&format!("$document{}", i), doc);
            }
            s = s.replace("$document", &first_doc);
            let rendered_arg = anodizer_core::template::render(&s, &vars)
                .with_context(|| format!("sbom[{}]: failed to render arg template '{}'", id, s))?;
            rendered_args.push(rendered_arg);
        }

        let mut rendered_env: Vec<(String, String)> = Vec::with_capacity(env_vars.len());
        for (k, v) in &env_vars {
            let rendered_val = anodizer_core::template::render(v, &vars)
                .with_context(|| format!("sbom[{}]: failed to render env template '{}'", id, v))?;
            rendered_env.push((k.clone(), rendered_val));
        }

        log.verbose(&format!(
            "running {} {} (sbom[{}])",
            cmd,
            rendered_args.join(" "),
            id
        ));

        let mut command = Command::new(cmd);
        command.args(&rendered_args);
        command.current_dir(dist);
        command.env_clear();
        anodizer_core::util::apply_minimal_env(&mut command);
        for (k, v) in &rendered_env {
            command.env(k, v);
        }

        let output = command
            .output()
            .with_context(|| format!("sbom[{}]: failed to run '{}'", id, cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("sbom[{}]: '{}' failed: {}", id, cmd, stderr.trim());
        }

        let mut any_doc_found = false;
        for doc_path in &rendered_docs {
            // Each rendered document path is glob-expanded against `dist`,
            // for the SBOM document path. This lets a
            // user write `documents: ["*.spdx.json"]` and get a separate
            // registered artifact per matched file (e.g.
            // `myproj-1.0.spdx.json`), rather than one artifact whose name
            // is the literal glob pattern:
            // `Name: filepath.Base(match)` (NOT `filepath.Base(path)`).
            let full_pattern = dist.join(doc_path);
            let pattern_str = full_pattern.to_string_lossy().into_owned();
            let entries = glob::glob(&pattern_str).with_context(|| {
                format!(
                    "sbom[{}]: invalid glob pattern '{}' for document",
                    id, pattern_str
                )
            })?;

            for entry in entries {
                let match_path = entry.with_context(|| {
                    format!(
                        "sbom[{}]: failed to read glob match for '{}'",
                        id, pattern_str
                    )
                })?;
                if !match_path.exists() {
                    continue;
                }
                // Check the file is non-empty — a zero-byte SBOM is useless
                let file_len = std::fs::metadata(&match_path).map(|m| m.len()).unwrap_or(0);
                if file_len == 0 {
                    bail!(
                        "sbom[{}]: command succeeded but produced empty output file '{}'",
                        id,
                        match_path.display()
                    );
                }
                any_doc_found = true;

                let mut metadata = HashMap::new();
                metadata.insert("sbom_id".to_string(), id.to_string());
                // Subject provenance: the SBOM inherits its subject's
                // verdict record so the release `ids:` filter gives it the
                // same upload verdict as the artifact it catalogs.
                if let Some(kind) = artifact_kind {
                    let (subject_kind, inherited_id) =
                        anodizer_core::artifact::subject_verdict_record(*kind, artifact_meta);
                    if let Some(subject_kind) = subject_kind {
                        metadata.insert(
                            anodizer_core::artifact::SUBJECT_KIND_META.to_string(),
                            subject_kind,
                        );
                    }
                    if let Some(subject_id) = inherited_id {
                        metadata.insert("id".to_string(), subject_id);
                    }
                }

                let name = match_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Sbom,
                    name,
                    path: match_path,
                    target: None,
                    crate_name: project_name.clone(),
                    metadata,
                    size: None,
                });
            }
        }
        if !any_doc_found {
            bail!(
                "sbom[{}]: command '{}' succeeded but produced no output files (expected: {:?})",
                id,
                cmd,
                rendered_docs
            );
        }
    }

    Ok(())
}

/// Built-in SBOM generation using Cargo.lock parsing (CycloneDX/SPDX).
///
/// Iterates the same artifact-filter shape as the external (syft) path:
///
/// * `artifacts: any` (or unset) → one SBOM at the
///   `<project>-<version>.<ext>` legacy filename, no per-artifact template
///   rendering.
/// * `artifacts: archive|binary|source|package|diskimage|installer` →
///   STILL one workspace SBOM at `<project>-<version>.<ext>`, NOT one per
///   matched artifact. The built-in output is Cargo.lock-derived (it catalogs
///   the workspace dependency graph), so it is archive-independent; emitting N
///   differently-named copies of identical bytes would only multiply the
///   downstream checksum + signature object count. The matched-artifact scan
///   is still load-bearing: it gates the strict-guard (an `archive` SBOM
///   configured against a build that produced none is a config bug) and the
///   first match supplies the subject verdict record the release `ids:` filter
///   inherits. The emitted document is target-independent (`target: None`) for
///   the same reason — see the registration site below.
pub(crate) fn run_sbom_builtin(
    ctx: &mut Context,
    dist: &Path,
    sbom_cfg: &SbomConfig,
    project_name: &str,
    version: &str,
) -> Result<()> {
    let log = ctx.logger("sbom");
    let id = sbom_cfg.resolved_id();
    let artifacts_type = sbom_cfg.resolved_artifacts();
    let documents = sbom_cfg.resolved_documents(artifacts_type);

    let (format, builtin_extension) = builtin_format_and_extension(&documents);

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would generate {} SBOM for {} (sbom[{}])",
            format, project_name, id
        ));
        return Ok(());
    }

    let search_dir = get_repo_root(&log)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cargo_lock_path = find_cargo_lock(&search_dir)?;
    let cargo_lock_content = std::fs::read_to_string(&cargo_lock_path).with_context(|| {
        format!(
            "sbom: failed to read Cargo.lock at {}",
            cargo_lock_path.display()
        )
    })?;

    let packages = parse_cargo_lock(&cargo_lock_content)?;
    log.status(&format!(
        "parsed {} packages from Cargo.lock (sbom[{}])",
        packages.len(),
        id
    ));

    // Deterministic inputs: the same release tag must produce byte-identical
    // SBOM output across pipeline retries, otherwise GitHub ReleaseAsset
    // rejects the re-upload with `already_exists` (size mismatch).
    //
    // Resolution order:
    //   1. `ctx.determinism.sde` — the canonical SOURCE_DATE_EPOCH seeded by
    //      `BuildStage` (or whatever stage runs first under
    //      `resolve_reproducible_epoch`). This is the load-bearing path
    //      under the release-resilience determinism contract.
    //   2. `CommitDate` template var — fallback for runs where the
    //      determinism state was not seeded (e.g. SBOM-only commands).
    //   3. `anodizer_core::sde::resolve_now()` — last-resort fallback.
    //      `resolve_now` itself honors `SOURCE_DATE_EPOCH` so an external
    //      reproducibility harness (debian builders, nix, etc.) still
    //      gets a stable timestamp without the in-process determinism
    //      state being seeded.
    let timestamp = if let Some(state) = ctx.determinism.as_ref() {
        chrono::DateTime::<chrono::Utc>::from_timestamp(state.sde, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| {
                log.warn(&format!(
                    "SOURCE_DATE_EPOCH {} out of range for sbom[{}]; falling back to CommitDate",
                    state.sde, id
                ));
                ctx.template_vars()
                    .get("CommitDate")
                    .cloned()
                    .unwrap_or_else(|| anodizer_core::sde::resolve_now().to_rfc3339())
            })
    } else if let Some(cd) = ctx.template_vars().get("CommitDate").cloned() {
        cd
    } else {
        log.warn(&format!(
            "no SOURCE_DATE_EPOCH or CommitDate for sbom[{}]; SBOM timestamp will not be reproducible",
            id
        ));
        anodizer_core::sde::resolve_now().to_rfc3339()
    };
    let namespace_uuid = deterministic_uuid_from(&format!("{}-{}", project_name, version));

    let extension = builtin_extension;
    let sbom_json = match format {
        "spdx" => generate_spdx(
            project_name,
            version,
            &timestamp,
            &namespace_uuid,
            &packages,
        )?,
        _ => generate_cyclonedx(project_name, version, &timestamp, &packages)?,
    };

    let json_string = serde_json::to_string_pretty(&sbom_json)
        .context("sbom: failed to serialize SBOM to JSON")?;

    // Filter artifacts to write the SBOM next to. Mirrors the external
    // (syft) path's artifact-filter shape so swapping `cmd:` in and out
    // of the config doesn't change the user-visible artifact set.
    let matching_artifacts: Vec<SbomSubject> = match artifacts_type {
        "any" => vec![(PathBuf::new(), HashMap::new(), None, None)],
        "binary" => {
            let candidates = ctx.artifacts.binary_like_dedup();
            let pre_ids = candidates.len();
            let matched: Vec<SbomSubject> = candidates
                .into_iter()
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| {
                    (
                        a.path.clone(),
                        a.metadata.clone(),
                        a.target.clone(),
                        Some(a.kind),
                    )
                })
                .collect();
            warn_ids_eliminated_all(&log, id, sbom_cfg.ids.as_deref(), pre_ids, matched.len());
            matched
        }
        _ => {
            let kind = typed_artifact_kind(artifacts_type, id)?;
            // A macOS `.app` bundle registers as Installer + format=appbundle but
            // is a DIRECTORY never uploaded raw; skip it so the native SBOM path
            // mirrors the syft path and emits no SBOM for a never-shipped asset.
            let pre_ids = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .count();
            let matched: Vec<SbomSubject> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| {
                    (
                        a.path.clone(),
                        a.metadata.clone(),
                        a.target.clone(),
                        Some(a.kind),
                    )
                })
                .collect();
            warn_ids_eliminated_all(&log, id, sbom_cfg.ids.as_deref(), pre_ids, matched.len());
            matched
        }
    };

    if matching_artifacts.is_empty() {
        // Mirror the external path's strict-guard behavior: a configured
        // SBOM that matches zero artifacts is a config bug under strict
        // mode, a silent skip under non-strict.
        ctx.strict_guard(
            &log,
            &format!(
                "skipped SBOM generation — no matching '{}' artifacts found (sbom[{}])",
                artifacts_type, id
            ),
        )?;
        return Ok(());
    }

    // The built-in (Cargo.lock) generator's output is archive-INDEPENDENT by
    // construction — it catalogs the workspace dependency graph, not the
    // contents of any one archive. Emitting one document per matched artifact
    // would write the SAME bytes to N differently-named files (N redundant
    // checksum + signature objects). So emit ONE workspace SBOM regardless of
    // `artifacts:` mode, named `<project>-<version>.<ext>` (the `any`
    // filename). The matched-artifact scan above is still load-bearing: it
    // gates the strict-guard (an `archive` SBOM configured against a build that
    // produced none is a config bug) and the first matched subject carries the
    // verdict record the release `ids:` filter inherits. External (syft)
    // scanning DOES vary per archive and never reaches this function.
    // Only the first match's metadata + kind are consumed (verdict record /
    // `ids:` inheritance); its `target` is deliberately NOT propagated — the
    // workspace SBOM is target-independent (registered `target: None` below).
    let (_, subject_meta, _subject_target, subject_kind) = &matching_artifacts[0];

    let filename = format!("{}-{}.{}", project_name, version, extension);
    let output_path = dist.join(filename);

    std::fs::write(&output_path, &json_string)
        .with_context(|| format!("sbom: failed to write {}", output_path.display()))?;

    let name = output_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    log.status(&format!("wrote {} for sbom[{}] ({})", name, id, format));

    let mut metadata = HashMap::new();
    metadata.insert("format".to_string(), format.to_string());
    metadata.insert("sbom_id".to_string(), id.to_string());
    // Subject provenance: the SBOM inherits its subject's verdict record so
    // the release `ids:` filter gives it the same upload verdict as the
    // artifact it catalogs.
    if let Some(kind) = subject_kind {
        let (verdict_kind, inherited_id) =
            anodizer_core::artifact::subject_verdict_record(*kind, subject_meta);
        if let Some(verdict_kind) = verdict_kind {
            metadata.insert(
                anodizer_core::artifact::SUBJECT_KIND_META.to_string(),
                verdict_kind,
            );
        }
        if let Some(subject_id) = inherited_id {
            metadata.insert("id".to_string(), subject_id);
        }
    }

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Sbom,
        name,
        path: output_path,
        // Target-INDEPENDENT by construction: this document catalogs the
        // workspace dependency graph, not any one archive, and every shard of
        // a sharded release produces byte-identical output. Stamping it with
        // `subject_target` would give each shard a different target on the
        // same path, so the per-shard manifest merge's
        // `dedupe_targetless_duplicates` (which only collapses `None`) leaves
        // N copies and the duplicate-path guard rejects them. Subject
        // provenance the `ids:` filter needs is carried in `metadata`, not here.
        target: None,
        crate_name: project_name.to_string(),
        metadata,
        size: None,
    });

    Ok(())
}

/// Environment requirements for the sbom stage: each active `sboms:` entry's
/// generator command (default `syft`) plus env vars referenced by its
/// templated args/env. Entries whose `skip` evaluates true are inert.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::template_env_refs;
    let mut out = Vec::new();
    for cfg in &ctx.config.sboms {
        let skipped = cfg.skip.as_ref().is_some_and(|s| {
            s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        });
        if skipped {
            continue;
        }
        out.push(anodizer_core::EnvRequirement::Tool {
            name: cfg.resolved_cmd().to_string(),
        });
        for s in cfg.args.iter().flatten().chain(cfg.env.iter().flatten()) {
            let refs = template_env_refs(s);
            if !refs.is_empty() {
                out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
            }
        }
    }
    out
}
