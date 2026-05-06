//! `SourceStage` orchestration: source archive emission and SBOM generation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::{SbomConfig, SourceFileEntry};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::archive::{SourceArchiveInputs, create_source_archive, get_repo_root};
use crate::sbom::{deterministic_uuid_from, generate_cyclonedx, generate_spdx, parse_cargo_lock};

pub struct SourceStage;

impl Stage for SourceStage {
    fn name(&self) -> &str {
        "source"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("source");
        let source_enabled = ctx
            .config
            .source
            .as_ref()
            .map(|s| s.is_enabled())
            .unwrap_or(false);

        if !source_enabled {
            log.status("source archive not enabled, skipping");
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        if !ctx.is_dry_run() {
            std::fs::create_dir_all(&dist).with_context(|| {
                format!("source: failed to create dist dir: {}", dist.display())
            })?;
        }

        self.run_source_archive(ctx, &dist)?;

        Ok(())
    }
}

impl SourceStage {
    fn run_source_archive(&self, ctx: &mut Context, dist: &Path) -> Result<()> {
        let source_cfg = ctx
            .config
            .source
            .as_ref()
            .context("source stage invoked without source config (programmer bug)")?;
        let format = source_cfg.archive_format().to_string();

        // Determine the archive name
        let project_name = &ctx.config.project_name;
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let name = if let Some(ref tpl) = source_cfg.name_template {
            ctx.render_template(tpl)
                .with_context(|| format!("source: failed to render name_template '{}'", tpl))?
        } else {
            format!("{}-{}", project_name, version)
        };

        // Determine the archive prefix (directory name inside the archive).
        // GoReleaser defaults to empty (no prefix) when prefix_template is not configured.
        let prefix = if let Some(ref tpl) = source_cfg.prefix_template {
            ctx.render_template(tpl)
                .with_context(|| format!("source: failed to render prefix_template '{}'", tpl))?
        } else {
            String::new()
        };

        let log = ctx.logger("source");

        let cwd = ctx
            .options
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo_root = get_repo_root(&cwd)?;

        // Render and expand extra-file globs up front, even in dry-run mode,
        // so users catch template typos and zero-match patterns before the
        // real run.
        let mut extra_files: Vec<SourceFileEntry> = Vec::new();
        for entry in &source_cfg.files {
            let rendered_src = ctx.render_template(&entry.src).with_context(|| {
                format!("source: render extra files src template '{}'", entry.src)
            })?;

            let pattern = if Path::new(&rendered_src).is_absolute() {
                rendered_src.clone()
            } else {
                repo_root.join(&rendered_src).to_string_lossy().into_owned()
            };

            let expanded_for_entry = match glob::glob(&pattern) {
                Ok(paths) => {
                    let expanded: Vec<_> = paths
                        .filter_map(|p| p.ok())
                        .filter(|p| p.is_file())
                        .map(|p| SourceFileEntry {
                            src: p.to_string_lossy().into_owned(),
                            dst: entry.dst.clone(),
                            strip_parent: entry.strip_parent,
                            info: entry.info.clone(),
                        })
                        .collect();
                    if expanded.is_empty() {
                        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
                            log.warn(&format!(
                                "source: extra file pattern {pattern:?} matched no files"
                            ));
                        }
                        vec![SourceFileEntry {
                            src: rendered_src,
                            dst: entry.dst.clone(),
                            strip_parent: entry.strip_parent,
                            info: entry.info.clone(),
                        }]
                    } else {
                        expanded
                    }
                }
                Err(e) => {
                    log.warn(&format!(
                        "source: extra file pattern {pattern:?} is not a valid glob ({e}); \
                         treating as literal path"
                    ));
                    vec![SourceFileEntry {
                        src: rendered_src,
                        dst: entry.dst.clone(),
                        strip_parent: entry.strip_parent,
                        info: entry.info.clone(),
                    }]
                }
            };
            extra_files.extend(expanded_for_entry);
        }

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would create {}.{} archive",
                name, format
            ));
            return Ok(());
        }

        log.status(&format!("creating {}.{} archive...", name, format));
        let commit = ctx
            .git_info
            .as_ref()
            .map(|info| info.commit.as_str())
            .unwrap_or("HEAD");
        let output_path = create_source_archive(&SourceArchiveInputs {
            dist,
            format: &format,
            name: &name,
            prefix: &prefix,
            extra_files: &extra_files,
            repo_root: &repo_root,
            commit,
            log: &log,
            strict: ctx.is_strict(),
        })?;

        // GoReleaser sets artifact name to the filename (e.g. "foo-1.0.0.tar.gz").
        let artifact_name = output_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), format);

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::SourceArchive,
            name: artifact_name,
            path: output_path,
            target: None,
            crate_name: project_name.clone(),
            metadata,
            size: None,
        });

        Ok(())
    }

    // SBOM generation has been extracted to the standalone stage-sbom crate.
    // Kept as dead code temporarily for reference; the run_sbom method is now
    // implemented in anodizer_stage_sbom::SbomStage.
    #[allow(dead_code)]
    fn run_sbom(&self, ctx: &mut Context, dist: &Path, sbom_cfg: &SbomConfig) -> Result<()> {
        let log = ctx.logger("source");
        let project_name = ctx.config.project_name.clone();
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let id = sbom_cfg.id.as_deref().unwrap_or("default");

        // Evaluate skip — supports bool or template string
        if let Some(ref d) = sbom_cfg.skip {
            let off = d
                .try_evaluates_to_true(|s| ctx.render_template(s))
                .with_context(|| format!("sbom[{}]: render skip template", id))?;
            if off {
                log.status(&format!("sbom[{}]: skipped", id));
                return Ok(());
            }
        }

        // Determine if this is a built-in (no external command) or subprocess model
        let use_builtin = sbom_cfg.cmd.is_none() && sbom_cfg.args.is_none();

        if use_builtin {
            return self.run_sbom_builtin(ctx, dist, sbom_cfg, &project_name, &version);
        }

        // --- External command (subprocess) model ---
        let cmd = sbom_cfg.cmd.as_deref().unwrap_or("syft");
        let artifacts_type = sbom_cfg.artifacts.as_deref().unwrap_or("archive");

        // Default documents based on artifacts type
        let documents = sbom_cfg
            .documents
            .clone()
            .unwrap_or_else(|| match artifacts_type {
                "binary" => {
                    vec!["{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}.sbom.json".to_string()]
                }
                "any" => vec![],
                _ => vec!["{{ .ArtifactName }}.sbom.json".to_string()],
            });

        // Default args for syft
        let args = sbom_cfg.args.clone().unwrap_or_else(|| {
            if cmd == "syft" {
                vec![
                    "$artifact".to_string(),
                    "--output".to_string(),
                    "spdx-json=$document".to_string(),
                    "--enrich".to_string(),
                    "all".to_string(),
                ]
            } else {
                vec![]
            }
        });

        // Default env for syft with source/archive
        let env_vars: Vec<(String, String)> = match sbom_cfg.env.as_deref() {
            Some(list) => anodizer_core::config::parse_env_entries(list)
                .with_context(|| "source-sbom: parse env entries")?,
            None => {
                if cmd == "syft" && matches!(artifacts_type, "source" | "archive") {
                    vec![(
                        "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
                        "true".to_string(),
                    )]
                } else {
                    Vec::new()
                }
            }
        };

        // Filter artifacts from the registry based on artifacts type
        let matching_artifacts: Vec<(PathBuf, HashMap<String, String>, Option<String>)> =
            match artifacts_type {
                "any" => vec![], // "any" calls once with no specific artifact
                _ => {
                    let kind = match artifacts_type {
                        "source" => ArtifactKind::SourceArchive,
                        "archive" => ArtifactKind::Archive,
                        "binary" => ArtifactKind::Binary,
                        "package" => ArtifactKind::LinuxPackage,
                        "diskimage" => ArtifactKind::DiskImage,
                        "installer" => ArtifactKind::Installer,
                        _ => {
                            log.warn(&format!(
                                "sbom[{}]: unknown artifacts type '{}', defaulting to archive",
                                id, artifacts_type
                            ));
                            ArtifactKind::Archive
                        }
                    };

                    let matched: Vec<(PathBuf, HashMap<String, String>, Option<String>)> = ctx
                        .artifacts
                        .all()
                        .iter()
                        .filter(|a| a.kind == kind)
                        .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                        .map(|a| (a.path.clone(), a.metadata.clone(), a.target.clone()))
                        .collect();

                    if matched.is_empty() {
                        log.status(&format!(
                            "sbom[{}]: no matching '{}' artifacts found, skipping",
                            id, artifacts_type
                        ));
                        return Ok(());
                    }

                    matched
                }
            };

        if ctx.is_dry_run() {
            if artifacts_type == "any" {
                log.status(&format!(
                    "(dry-run) sbom[{}]: would run '{}' for all artifacts",
                    id, cmd
                ));
            } else {
                for (path, _, _) in &matching_artifacts {
                    log.status(&format!(
                        "(dry-run) sbom[{}]: would run '{}' on {}",
                        id,
                        cmd,
                        path.display()
                    ));
                }
            }
            return Ok(());
        }

        // For "any" type, run the command once with no specific artifact
        let artifact_list: Vec<(PathBuf, HashMap<String, String>, Option<String>)> =
            if artifacts_type == "any" {
                vec![(PathBuf::new(), HashMap::new(), None)]
            } else {
                matching_artifacts
            };

        for (artifact_path, artifact_meta, artifact_target) in &artifact_list {
            let artifact_rel = if artifact_path.as_os_str().is_empty() {
                String::new()
            } else {
                artifact_path
                    .strip_prefix(dist)
                    .unwrap_or(artifact_path)
                    .display()
                    .to_string()
            };

            // Set per-artifact template vars for document template rendering
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact");
            ctx.template_vars_mut().set("ArtifactName", artifact_name);
            ctx.template_vars_mut().set(
                "ArtifactExt",
                anodizer_core::template::extract_artifact_ext(artifact_name),
            );
            // Set ArtifactID from artifact metadata "id" key (Pro addition)
            ctx.template_vars_mut().set(
                "ArtifactID",
                artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
            );

            // If artifact has target info, set Os/Arch/Target
            if let Some(target) = artifact_target {
                let (os, arch) = anodizer_core::target::map_target(target);
                ctx.template_vars_mut().set("Os", &os);
                ctx.template_vars_mut().set("Arch", &arch);
                ctx.template_vars_mut().set("Target", target);
            } else if let Some(target) = artifact_meta.get("target") {
                let (os, arch) = anodizer_core::target::map_target(target);
                ctx.template_vars_mut().set("Os", &os);
                ctx.template_vars_mut().set("Arch", &arch);
                ctx.template_vars_mut().set("Target", target);
            }

            // Render document paths
            let mut rendered_docs: Vec<String> = Vec::new();
            for doc_tpl in &documents {
                let rendered = ctx.render_template(doc_tpl).with_context(|| {
                    format!(
                        "sbom[{}]: failed to render document template '{}'",
                        id, doc_tpl
                    )
                })?;
                rendered_docs.push(rendered);
            }

            let first_doc = rendered_docs.first().cloned().unwrap_or_default();

            // Render args — replace $artifactID, $artifact, $document0, $document1, etc.
            // IMPORTANT: Replace longer prefixes first ($artifactID before $artifact,
            // $documentN before $document) to avoid partial-match corruption.
            let artifact_id = artifact_meta.get("id").map(|s| s.as_str()).unwrap_or("");
            let mut rendered_args: Vec<String> = Vec::with_capacity(args.len());
            for arg in &args {
                let mut s = arg.replace("$artifactID", artifact_id);
                s = s.replace("$artifact", &artifact_rel);
                // Replace numbered $documentN FIRST (before bare $document)
                for (i, doc) in rendered_docs.iter().enumerate() {
                    s = s.replace(&format!("$document{}", i), doc);
                }
                // Then replace bare $document (won't match already-replaced $documentN)
                s = s.replace("$document", &first_doc);
                // Render template vars in args
                let rendered_arg = ctx.render_template(&s).with_context(|| {
                    format!("sbom[{}]: failed to render arg template '{}'", id, s)
                })?;
                rendered_args.push(rendered_arg);
            }

            // Render env vars
            let mut rendered_env: Vec<(String, String)> = Vec::with_capacity(env_vars.len());
            for (k, v) in &env_vars {
                let rendered_val = ctx.render_template(v).with_context(|| {
                    format!("sbom[{}]: failed to render env template '{}'", id, v)
                })?;
                rendered_env.push((k.clone(), rendered_val));
            }

            log.status(&format!(
                "sbom[{}]: running {} {}",
                id,
                cmd,
                rendered_args.join(" ")
            ));

            let mut command = Command::new(cmd);
            command.args(&rendered_args);
            command.current_dir(dist);
            // restrict environment to a small
            // whitelist to prevent accidental leakage of tokens/credentials.
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

            // Register each output document as an SBOM artifact
            for doc_path in &rendered_docs {
                let full_path = dist.join(doc_path);
                if full_path.exists() {
                    let mut metadata = HashMap::new();
                    metadata.insert("sbom_id".to_string(), id.to_string());

                    ctx.artifacts.add(Artifact {
                        kind: ArtifactKind::Sbom,
                        name: String::new(),
                        path: full_path,
                        target: None,
                        crate_name: project_name.clone(),
                        metadata,
                        size: None,
                    });
                }
            }
        }

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Target", "");
        ctx.template_vars_mut().set("ArtifactName", "");
        ctx.template_vars_mut().set("ArtifactExt", "");
        ctx.template_vars_mut().set("ArtifactID", "");

        Ok(())
    }

    /// Built-in SBOM generation using Cargo.lock parsing (CycloneDX/SPDX).
    /// Used when no external command is configured.
    fn run_sbom_builtin(
        &self,
        ctx: &mut Context,
        dist: &Path,
        sbom_cfg: &SbomConfig,
        project_name: &str,
        version: &str,
    ) -> Result<()> {
        let log = ctx.logger("source");
        let id = sbom_cfg.id.as_deref().unwrap_or("default");

        // Determine format from documents hint or default to cyclonedx
        let format = if let Some(ref docs) = sbom_cfg.documents {
            if docs.iter().any(|d| d.to_lowercase().contains("spdx")) {
                "spdx"
            } else {
                "cyclonedx"
            }
        } else {
            "cyclonedx"
        };

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) sbom[{}]: would generate {} SBOM for {}",
                id, format, project_name
            ));
            return Ok(());
        }

        // Find Cargo.lock starting from repo root (or CWD as fallback)
        let fallback_cwd = ctx
            .options
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let search_dir = get_repo_root(&fallback_cwd).unwrap_or(fallback_cwd);
        let cargo_lock_path = find_cargo_lock(&search_dir)?;
        let cargo_lock_content = std::fs::read_to_string(&cargo_lock_path).with_context(|| {
            format!(
                "sbom: failed to read Cargo.lock at {}",
                cargo_lock_path.display()
            )
        })?;

        let packages = parse_cargo_lock(&cargo_lock_content)?;
        log.status(&format!(
            "sbom[{}]: parsed {} packages from Cargo.lock",
            id,
            packages.len()
        ));

        // Deterministic inputs: the same release tag must produce byte-identical
        // SBOM output across pipeline retries, otherwise GitHub ReleaseAsset
        // rejects the re-upload with `already_exists` (size mismatch).
        let timestamp = ctx
            .template_vars()
            .get("CommitDate")
            .cloned()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let namespace_uuid = deterministic_uuid_from(&format!("{}-{}", project_name, version));

        let (sbom_json, extension) = match format {
            "cyclonedx" => {
                let sbom = generate_cyclonedx(project_name, version, &timestamp, &packages)?;
                (sbom, "cdx.json")
            }
            "spdx" => {
                let sbom = generate_spdx(
                    project_name,
                    version,
                    &timestamp,
                    &namespace_uuid,
                    &packages,
                )?;
                (sbom, "spdx.json")
            }
            _ => bail!(
                "sbom[{}]: unsupported format '{}' (use cyclonedx or spdx)",
                id,
                format
            ),
        };

        let filename = format!("{}-{}.{}", project_name, version, extension);
        let output_path = dist.join(&filename);

        let json_string = serde_json::to_string_pretty(&sbom_json)
            .context("sbom: failed to serialize SBOM to JSON")?;
        std::fs::write(&output_path, &json_string)
            .with_context(|| format!("sbom: failed to write {}", output_path.display()))?;

        log.status(&format!("sbom[{}]: wrote {} ({})", id, filename, format));

        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), format.to_string());
        metadata.insert("sbom_id".to_string(), id.to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Sbom,
            name: String::new(),
            path: output_path,
            target: None,
            crate_name: project_name.to_string(),
            metadata,
            size: None,
        });

        Ok(())
    }
}

/// Search for Cargo.lock starting from `start_dir` and walking up parent directories.
fn find_cargo_lock(start_dir: &Path) -> Result<PathBuf> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join("Cargo.lock");
        if candidate.exists() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    bail!(
        "sbom: Cargo.lock not found starting from '{}' or any parent directory",
        start_dir.display()
    )
}
