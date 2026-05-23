//! SBOM (Software Bill of Materials) generation stage for anodizer.
//!
//! Supports two modes:
//! 1. **Built-in**: Parses `Cargo.lock` to generate CycloneDX 1.5 or SPDX 2.3 JSON.
//!    This is a Rust-specific value-add not present in GoReleaser.
//! 2. **External command**: Runs an external tool (default: `syft`) to catalog artifacts.
//!    Matches GoReleaser's SBOM pipe behavior exactly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::SbomConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// Built-in SBOM generation (Rust-specific)
// ---------------------------------------------------------------------------

/// A parsed Cargo.lock package entry.
pub struct CargoPackage {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
}

/// Parse `Cargo.lock` to extract package entries.
pub fn parse_cargo_lock(content: &str) -> Result<Vec<CargoPackage>> {
    let parsed: toml::Value =
        toml::from_str(content).context("sbom: failed to parse Cargo.lock as TOML")?;

    let packages = parsed
        .get("package")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?.to_string();
                    let version = entry.get("version")?.as_str()?.to_string();
                    let source = entry
                        .get("source")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string());
                    Some(CargoPackage {
                        name,
                        version,
                        source,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(packages)
}

/// Generate a CycloneDX 1.5 SBOM in JSON format.
///
/// `timestamp` is embedded in `metadata.timestamp` and must be supplied by the
/// caller so that repeated pipeline runs (e.g. anodizer-action retries) emit
/// byte-identical output. Callers should derive it from `ctx.template_vars()`
/// (`CommitDate`) so the value is tied to the release tag, not wall-clock.
pub fn generate_cyclonedx(
    project_name: &str,
    version: &str,
    timestamp: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    let components: Vec<serde_json::Value> = packages
        .iter()
        .map(|pkg| {
            let mut component = serde_json::json!({
                "type": "library",
                "name": pkg.name,
                "version": pkg.version,
                "purl": format!("pkg:cargo/{}@{}", pkg.name, pkg.version),
            });

            if let Some(ref source) = pkg.source
                && source.starts_with("registry+")
            {
                component["externalReferences"] = serde_json::json!([
                    {
                        "type": "distribution",
                        "url": format!("https://crates.io/crates/{}/{}", pkg.name, pkg.version)
                    }
                ]);
            }

            component
        })
        .collect();

    let sbom = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "component": {
                "type": "application",
                "name": project_name,
                "version": version,
            },
            "tools": {
                "components": [
                    {
                        "type": "application",
                        "name": "anodizer",
                        "publisher": "anodizer",
                    }
                ]
            }
        },
        "components": components,
    });

    Ok(sbom)
}

/// Generate an SPDX 2.3 SBOM in JSON format.
///
/// `timestamp` populates `creationInfo.created`; `namespace_uuid` populates the
/// trailing segment of `documentNamespace`. Both are caller-supplied so the
/// output is byte-identical across repeated pipeline runs (release asset
/// uploads are non-idempotent when the file bytes differ from a prior
/// upload — GitHub's ReleaseAsset API rejects re-uploads with `already_exists`
/// when sizes diverge).
pub fn generate_spdx(
    project_name: &str,
    version: &str,
    timestamp: &str,
    namespace_uuid: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    let root_package = serde_json::json!({
        "SPDXID": "SPDXRef-Package",
        "name": project_name,
        "versionInfo": version,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": false,
    });

    let mut spdx_packages = vec![root_package];
    let mut relationships = vec![serde_json::json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relatedSpdxElement": "SPDXRef-Package",
        "relationshipType": "DESCRIBES",
    })];

    for (i, pkg) in packages.iter().enumerate() {
        let spdx_id = format!("SPDXRef-Package-{}", i);

        let download_location = if let Some(ref source) = pkg.source {
            if source.starts_with("registry+") {
                format!("https://crates.io/crates/{}/{}", pkg.name, pkg.version)
            } else {
                source.clone()
            }
        } else {
            "NOASSERTION".to_string()
        };

        let pkg_entry = serde_json::json!({
            "SPDXID": spdx_id,
            "name": pkg.name,
            "versionInfo": pkg.version,
            "downloadLocation": download_location,
            "filesAnalyzed": false,
            "externalRefs": [
                {
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": format!("pkg:cargo/{}@{}", pkg.name, pkg.version),
                }
            ],
        });

        spdx_packages.push(pkg_entry);

        relationships.push(serde_json::json!({
            "spdxElementId": "SPDXRef-Package",
            "relatedSpdxElement": spdx_id,
            "relationshipType": "DEPENDS_ON",
        }));
    }

    let sbom = serde_json::json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": format!("{}-{}", project_name, version),
        "documentNamespace": format!(
            "https://spdx.org/spdxdocs/{}-{}-{}",
            project_name, version, namespace_uuid,
        ),
        "creationInfo": {
            "created": timestamp,
            "creators": ["Tool: anodizer"],
        },
        "packages": spdx_packages,
        "relationships": relationships,
    });

    Ok(sbom)
}

/// Deterministic UUID v4-shaped identifier derived from `seed`.
///
/// Same seed always produces the same UUID. Not cryptographic — the value is
/// only used as the trailing component of an SPDX `documentNamespace`, where
/// the purpose is per-document uniqueness within a project, not secrecy.
pub fn deterministic_uuid_from(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h1 = DefaultHasher::new();
    seed.hash(&mut h1);
    "anodizer-sbom-ns-v1".hash(&mut h1);
    let h1 = h1.finish();

    let mut h2 = DefaultHasher::new();
    seed.hash(&mut h2);
    "anodizer-sbom-ns-v2".hash(&mut h2);
    let h2 = h2.finish();

    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (h1 >> 32) as u32,
        (h1 >> 16) as u16,
        h1 as u16 & 0x0FFF,
        (h2 >> 48) as u16 & 0x3FFF | 0x8000,
        h2 & 0xFFFF_FFFF_FFFF,
    )
}

/// Search for Cargo.lock starting from `start_dir` and walking up parent directories.
pub fn find_cargo_lock(start_dir: &Path) -> Result<PathBuf> {
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

/// Get the repository root via `git rev-parse --show-toplevel`.
fn get_repo_root(log: &anodizer_core::log::StageLogger) -> Result<PathBuf> {
    log.debug("running: git rev-parse --show-toplevel");
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("sbom: failed to run git rev-parse")?;
    let output = log.check_output(output, "git rev-parse --show-toplevel")?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

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
            log.status("no SBOMs configured, skipping");
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
        log.status(&format!("sbom[{}]: skipped", id));
        return Ok(());
    }

    // Determine if this is a built-in (no external command) or subprocess model
    let use_builtin = sbom_cfg.cmd.is_none() && sbom_cfg.args.is_none();

    if use_builtin {
        return run_sbom_builtin(ctx, dist, sbom_cfg, &project_name, &version);
    }

    // --- External command (subprocess) model ---
    let cmd = sbom_cfg.resolved_cmd();
    let artifacts_type = sbom_cfg.resolved_artifacts();

    let documents = sbom_cfg.resolved_documents(artifacts_type);

    // when artifacts != "any", multiple
    // SBOM output documents are unsupported because each document name is
    // rendered per-artifact and would clobber on collision.
    if artifacts_type != "any" && documents.len() > 1 {
        anyhow::bail!(
            "sbom[{}]: multiple SBOM outputs when artifacts={:?} is unsupported",
            id,
            artifacts_type
        );
    }

    let args = sbom_cfg.resolved_args(cmd);

    let env_vars: Vec<(String, String)> = match sbom_cfg.env.as_deref() {
        Some(list) => anodizer_core::config::parse_env_entries(list)
            .with_context(|| "sbom: parse env entries")?,
        None => SbomConfig::default_syft_env_for(cmd, artifacts_type),
    };

    // Filter artifacts from the registry based on artifacts type.
    //
    // For `artifacts: binary` we match Binary + UploadableBinary + UniversalBinary
    // and dedup by path, preferring UploadableBinary (this mirrors GoReleaser's
    // `artifact.ByBinaryLikeArtifacts`: `internal/artifact/artifact.go:733-761`).
    // Without this, each per-arch Binary *plus* its UploadableBinary registration
    // would produce its own SBOM at the same path, causing file collisions.
    let matching_artifacts: Vec<(PathBuf, HashMap<String, String>, Option<String>)> =
        match artifacts_type {
            "any" => vec![],
            "binary" => ctx
                .artifacts
                .binary_like_dedup()
                .into_iter()
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| (a.path.clone(), a.metadata.clone(), a.target.clone()))
                .collect(),
            _ => {
                let kind = match artifacts_type {
                    "source" => ArtifactKind::SourceArchive,
                    "archive" => ArtifactKind::Archive,
                    "package" => ArtifactKind::LinuxPackage,
                    "diskimage" => ArtifactKind::DiskImage,
                    "installer" => ArtifactKind::Installer,
                    other => anyhow::bail!(
                        "sbom[{}]: unknown artifacts type '{}'. Valid values are: \
                         source, archive, package, diskimage, installer, binary, any",
                        id,
                        other
                    ),
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
                    ctx.strict_guard(
                        &log,
                        &format!(
                            "sbom[{}]: no matching '{}' artifacts found, skipping",
                            id, artifacts_type
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

        let artifact_name = artifact_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("artifact");
        ctx.template_vars_mut().set("ArtifactName", artifact_name);
        ctx.template_vars_mut().set(
            "ArtifactExt",
            anodizer_core::template::extract_artifact_ext(artifact_name),
        );
        ctx.template_vars_mut().set(
            "ArtifactID",
            artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
        );

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

        let mut rendered_docs: Vec<String> = Vec::new();
        for doc_tpl in &documents {
            let rendered = ctx.render_template(doc_tpl).with_context(|| {
                format!(
                    "sbom[{}]: failed to render document template '{}'",
                    id, doc_tpl
                )
            })?;
            // Document paths are joined onto `dist/` for both write and
            // artifact registration. An absolute path would silently bypass
            // dist (Path::join discards the base when joined with absolute)
            // and produce an artifact registered at a nonexistent
            // dist/$rendered location. GoReleaser refuses absolute paths
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
            let rendered_arg = ctx
                .render_template(&s)
                .with_context(|| format!("sbom[{}]: failed to render arg template '{}'", id, s))?;
            rendered_args.push(rendered_arg);
        }

        let mut rendered_env: Vec<(String, String)> = Vec::with_capacity(env_vars.len());
        for (k, v) in &env_vars {
            let rendered_val = ctx
                .render_template(v)
                .with_context(|| format!("sbom[{}]: failed to render env template '{}'", id, v))?;
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
            // matching GoReleaser's `internal/pipe/sbom/sbom.go`. This lets a
            // user write `documents: ["*.spdx.json"]` and get a separate
            // registered artifact per matched file (e.g.
            // `myproj-1.0.spdx.json`), rather than one artifact whose name
            // is the literal glob pattern. Mirrors GR commit 292203e:
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

    anodizer_core::template::clear_per_artifact_vars(ctx.template_vars_mut());

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
///   one SBOM per matched artifact, rendered through `documents[0]` so
///   the user gets the same per-archive layout (`<artifact>.cdx.json`)
///   syft would have produced. The SBOM *contents* are Cargo.lock-derived
///   and byte-identical across the iteration (every archive shares the
///   same workspace dependency graph), so the harness sees stable bytes
///   while consumers keep the per-artifact filename contract.
fn run_sbom_builtin(
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

    // Detect format from the document's extension chain rather than a raw
    // substring match. `mytool-spdx-companion.cdx.json` should resolve to
    // CycloneDX because the trailing extension is `.cdx.json`; the prior
    // `.contains("spdx")` heuristic flipped to SPDX based on the marketing
    // word in the basename and produced a malformed CycloneDX-by-name /
    // SPDX-by-payload file.
    let format = {
        let mut detected = "cyclonedx";
        for d in &documents {
            let lower = d.to_lowercase();
            if lower.ends_with(".spdx.json") || lower.ends_with(".spdx") {
                detected = "spdx";
                break;
            }
            if lower.ends_with(".cdx.json") || lower.ends_with(".cyclonedx.json") {
                detected = "cyclonedx";
                break;
            }
        }
        detected
    };

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) sbom[{}]: would generate {} SBOM for {}",
            id, format, project_name
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
        "sbom[{}]: parsed {} packages from Cargo.lock",
        id,
        packages.len()
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
                log.status(&format!(
                    "sbom[{}]: warn — SOURCE_DATE_EPOCH {} out of range; falling back to CommitDate",
                    id, state.sde
                ));
                ctx.template_vars()
                    .get("CommitDate")
                    .cloned()
                    .unwrap_or_else(|| anodizer_core::sde::resolve_now().to_rfc3339())
            })
    } else if let Some(cd) = ctx.template_vars().get("CommitDate").cloned() {
        cd
    } else {
        log.status(&format!(
            "sbom[{}]: warn — no SOURCE_DATE_EPOCH or CommitDate; SBOM timestamp will not be reproducible",
            id
        ));
        anodizer_core::sde::resolve_now().to_rfc3339()
    };
    let namespace_uuid = deterministic_uuid_from(&format!("{}-{}", project_name, version));

    let (sbom_json, extension) = match format {
        "cyclonedx" => (
            generate_cyclonedx(project_name, version, &timestamp, &packages)?,
            "cdx.json",
        ),
        "spdx" => (
            generate_spdx(
                project_name,
                version,
                &timestamp,
                &namespace_uuid,
                &packages,
            )?,
            "spdx.json",
        ),
        _ => bail!(
            "sbom[{}]: unsupported format '{}' (use cyclonedx or spdx)",
            id,
            format
        ),
    };

    let json_string = serde_json::to_string_pretty(&sbom_json)
        .context("sbom: failed to serialize SBOM to JSON")?;

    // Filter artifacts to write the SBOM next to. Mirrors the external
    // (syft) path's artifact-filter shape so swapping `cmd:` in and out
    // of the config doesn't change the user-visible artifact set.
    let matching_artifacts: Vec<(PathBuf, HashMap<String, String>, Option<String>)> =
        match artifacts_type {
            "any" => vec![(PathBuf::new(), HashMap::new(), None)],
            "binary" => ctx
                .artifacts
                .binary_like_dedup()
                .into_iter()
                .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                .map(|a| (a.path.clone(), a.metadata.clone(), a.target.clone()))
                .collect(),
            _ => {
                let kind = match artifacts_type {
                    "source" => ArtifactKind::SourceArchive,
                    "archive" => ArtifactKind::Archive,
                    "package" => ArtifactKind::LinuxPackage,
                    "diskimage" => ArtifactKind::DiskImage,
                    "installer" => ArtifactKind::Installer,
                    other => bail!(
                        "sbom[{}]: unknown artifacts type '{}'. Valid values are: \
                         source, archive, package, diskimage, installer, binary, any",
                        id,
                        other
                    ),
                };
                ctx.artifacts
                    .all()
                    .iter()
                    .filter(|a| a.kind == kind)
                    .filter(|a| matches_id_filter(a, sbom_cfg.ids.as_deref()))
                    .map(|a| (a.path.clone(), a.metadata.clone(), a.target.clone()))
                    .collect()
            }
        };

    if matching_artifacts.is_empty() {
        // Mirror the external path's strict-guard behavior: a configured
        // SBOM that matches zero artifacts is a config bug under strict
        // mode, a silent skip under non-strict.
        ctx.strict_guard(
            &log,
            &format!(
                "sbom[{}]: no matching '{}' artifacts found, skipping",
                id, artifacts_type
            ),
        )?;
        return Ok(());
    }

    // Track rendered output filenames so a misconfigured `documents:`
    // template (e.g. one missing `{{ .ArtifactName }}` while iterating
    // multiple archives) bails loudly rather than silently overwriting
    // the same file N times.
    let mut written_paths: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();

    for (artifact_path, artifact_meta, artifact_target) in &matching_artifacts {
        let output_path = if artifacts_type == "any" {
            // Legacy global-SBOM filename: `<project>-<version>.<ext>`.
            let filename = format!("{}-{}.{}", project_name, version, extension);
            dist.join(filename)
        } else {
            // Per-artifact: render `documents[0]` with `ArtifactName`
            // bound to the matched archive. Matches the external path's
            // template surface so config templates port verbatim.
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact");
            ctx.template_vars_mut().set("ArtifactName", artifact_name);
            ctx.template_vars_mut().set(
                "ArtifactExt",
                anodizer_core::template::extract_artifact_ext(artifact_name),
            );
            ctx.template_vars_mut().set(
                "ArtifactID",
                artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
            );
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

            let doc_tpl = documents.first().ok_or_else(|| {
                anyhow::anyhow!(
                    "sbom[{}]: built-in mode with `artifacts: {}` requires a `documents:` \
                     template (e.g. \"{{{{ .ArtifactName }}}}.cdx.json\")",
                    id,
                    artifacts_type
                )
            })?;
            let rendered = ctx.render_template(doc_tpl).with_context(|| {
                format!(
                    "sbom[{}]: failed to render document template '{}'",
                    id, doc_tpl
                )
            })?;
            if Path::new(&rendered).is_absolute() {
                bail!(
                    "sbom[{}]: rendered document path '{}' is absolute; \
                     SBOM outputs must be relative to the dist directory",
                    id,
                    rendered
                );
            }
            dist.join(rendered)
        };

        if !written_paths.insert(output_path.clone()) {
            bail!(
                "sbom[{}]: built-in mode rendered the same output path '{}' for two \
                 artifacts — add `{{{{ .ArtifactName }}}}` (or another per-artifact \
                 var) to the `documents:` template",
                id,
                output_path.display()
            );
        }

        std::fs::write(&output_path, &json_string)
            .with_context(|| format!("sbom: failed to write {}", output_path.display()))?;

        let name = output_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        log.status(&format!("sbom[{}]: wrote {} ({})", id, name, format));

        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), format.to_string());
        metadata.insert("sbom_id".to_string(), id.to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Sbom,
            name,
            path: output_path,
            target: artifact_target.clone(),
            crate_name: project_name.to_string(),
            metadata,
            size: None,
        });
    }

    anodizer_core::template::clear_per_artifact_vars(ctx.template_vars_mut());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use anodizer_core::artifact::ArtifactKind;
    #[cfg(unix)]
    use anodizer_core::config::SbomConfig;
    #[cfg(unix)]
    use anodizer_core::stage::Stage;
    #[cfg(unix)]
    use anodizer_core::test_helpers::TestContextBuilder;

    /// Regression for GoReleaser parity P8.1 (commit 292203e):
    /// when `documents:` contains a glob pattern that matches multiple
    /// files, each match must be registered as its own SBOM artifact
    /// using the matched filename — NOT the unexpanded glob pattern.
    ///
    /// Before the fix, `documents: ["*.spdx.json"]` produced (at most)
    /// one artifact whose `name` was the literal `*.spdx.json`, since
    /// the path was passed through `dist.join(...).file_name()` without
    /// glob expansion. Downstream stages (checksum, release-upload,
    /// signing) would then fail to find the file on disk.
    #[cfg(unix)]
    #[test]
    fn sbom_documents_glob_expands_to_matched_filenames() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let dist = tmpdir.path().to_path_buf();

        // Pre-create two files matching the glob, plus one that does
        // not, to assert filtering precision.
        std::fs::write(dist.join("alpha.spdx.json"), b"{\"a\":1}").unwrap();
        std::fs::write(dist.join("beta.spdx.json"), b"{\"b\":1}").unwrap();
        std::fs::write(dist.join("ignored.json"), b"{\"x\":1}").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .dist(dist.clone())
            .add_sbom(SbomConfig {
                id: Some("globbed".into()),
                cmd: Some("true".into()),
                args: Some(vec![]),
                documents: Some(vec!["*.spdx.json".into()]),
                artifacts: Some("any".into()),
                env: Some(vec![]),
                ..Default::default()
            })
            .build();

        SbomStage.run(&mut ctx).expect("sbom stage");

        let names: std::collections::BTreeSet<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();

        let expected: std::collections::BTreeSet<String> = ["alpha.spdx.json", "beta.spdx.json"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(
            names, expected,
            "SBOM artifact names must be the glob-matched filenames, \
             not the literal `*.spdx.json` pattern (GR 292203e)"
        );

        // Each matched file must register a distinct on-disk path.
        let paths: std::collections::BTreeSet<PathBuf> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.path.clone())
            .collect();
        assert_eq!(paths.len(), 2, "expected 2 distinct SBOM paths");
        for p in &paths {
            assert!(p.exists(), "registered SBOM path must exist: {:?}", p);
        }
    }

    // -----------------------------------------------------------------------
    // SOURCE_DATE_EPOCH wiring regression
    // -----------------------------------------------------------------------
    //
    // These tests pin the contract that the CycloneDX `metadata.timestamp`
    // field is derived from the run's SOURCE_DATE_EPOCH (via
    // `ctx.determinism.sde`), not wall-clock `Utc::now()`. Without this
    // wiring, two pipeline retries of the same release tag emit different
    // SBOM bytes and the second upload fails with GitHub ReleaseAsset
    // `already_exists` (size mismatch).

    /// `generate_cyclonedx` is byte-stable for the same `timestamp` input
    /// across repeated calls. Trivially true for a pure function, but
    /// pinned so a future refactor that introduces clock reads inside the
    /// generator (e.g. via `chrono::Utc::now()` in a helper) regresses
    /// the test.
    #[test]
    fn cyclonedx_output_byte_stable_for_same_timestamp() {
        let pkgs = vec![CargoPackage {
            name: "anyhow".into(),
            version: "1.0.0".into(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
        }];
        // RFC3339 derived from SDE 1_715_000_000 = 2024-05-06T12:53:20+00:00.
        let ts = "2024-05-06T12:53:20+00:00";
        let a = generate_cyclonedx("myproj", "1.2.3", ts, &pkgs).unwrap();
        let b = generate_cyclonedx("myproj", "1.2.3", ts, &pkgs).unwrap();
        let a_bytes = serde_json::to_vec_pretty(&a).unwrap();
        let b_bytes = serde_json::to_vec_pretty(&b).unwrap();
        assert_eq!(
            a_bytes, b_bytes,
            "CycloneDX output must be byte-identical for the same SDE-derived timestamp"
        );
    }

    /// Pins the SDE-to-RFC3339 conversion that `run_sbom_builtin` uses on
    /// `ctx.determinism.sde`. If this conversion drifts (e.g. UTC vs
    /// local TZ, seconds vs millis), the SBOM `metadata.timestamp` field
    /// changes and breaks retry idempotency.
    #[test]
    fn sbom_metadata_timestamp_honors_sde() {
        let sde: i64 = 1_715_000_000;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(sde, 0)
            .expect("SDE 1_715_000_000 is in range");
        let derived = dt.to_rfc3339();
        // The exact RFC3339 form chrono emits for this SDE — pinned so a
        // future chrono version that flips +00:00 -> Z (or vice-versa)
        // breaks this test instead of silently breaking SBOM byte
        // stability.
        assert_eq!(derived, "2024-05-06T12:53:20+00:00");

        // The generated SBOM embeds exactly that string in metadata.timestamp.
        let pkgs: Vec<CargoPackage> = vec![];
        let sbom = generate_cyclonedx("p", "0", &derived, &pkgs).unwrap();
        let embedded = sbom
            .get("metadata")
            .and_then(|m| m.get("timestamp"))
            .and_then(|t| t.as_str())
            .expect("metadata.timestamp present");
        assert_eq!(embedded, "2024-05-06T12:53:20+00:00");
    }

    /// Different SDEs produce different metadata timestamps (sanity: the
    /// timestamp is not pinned to a constant). Pair test for
    /// `sbom_metadata_timestamp_honors_sde`.
    #[test]
    fn sbom_metadata_timestamp_varies_with_sde() {
        let pkgs: Vec<CargoPackage> = vec![];
        let t1 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_715_000_000, 0)
            .unwrap()
            .to_rfc3339();
        let t2 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_716_000_000, 0)
            .unwrap()
            .to_rfc3339();
        assert_ne!(t1, t2);
        let s1 = generate_cyclonedx("p", "0", &t1, &pkgs).unwrap();
        let s2 = generate_cyclonedx("p", "0", &t2, &pkgs).unwrap();
        assert_ne!(
            serde_json::to_vec(&s1).unwrap(),
            serde_json::to_vec(&s2).unwrap(),
            "different SDEs must produce different SBOM bytes"
        );
    }
}
