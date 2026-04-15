//! SBOM (Software Bill of Materials) generation stage for anodize.
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

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::SbomConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

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
pub fn generate_cyclonedx(
    project_name: &str,
    version: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    let timestamp = chrono::Utc::now().to_rfc3339();

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
                        "name": "anodize",
                        "publisher": "anodize",
                    }
                ]
            }
        },
        "components": components,
    });

    Ok(sbom)
}

/// Generate an SPDX 2.3 SBOM in JSON format.
pub fn generate_spdx(
    project_name: &str,
    version: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    let timestamp = chrono::Utc::now().to_rfc3339();

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
            project_name,
            version,
            uuid_v4_simple()
        ),
        "creationInfo": {
            "created": timestamp,
            "creators": ["Tool: anodize"],
        },
        "packages": spdx_packages,
        "relationships": relationships,
    });

    Ok(sbom)
}

/// Simple UUID v4-shaped generation without pulling in a uuid crate.
fn uuid_v4_simple() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    let h1 = hasher.finish();

    let mut hasher2 = DefaultHasher::new();
    h1.hash(&mut hasher2);
    42u64.hash(&mut hasher2);
    let h2 = hasher2.finish();

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
fn get_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("sbom: failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("sbom: git rev-parse --show-toplevel failed");
    }
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
            let id = cfg.id.as_deref().unwrap_or("default");
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

    let id = sbom_cfg.id.as_deref().unwrap_or("default");

    // Evaluate disable — supports bool or template string
    if let Some(ref d) = sbom_cfg.disable
        && d.is_disabled(|s| ctx.render_template(s))
    {
        log.status(&format!("sbom[{}]: disabled, skipping", id));
        return Ok(());
    }

    // Determine if this is a built-in (no external command) or subprocess model
    let use_builtin = sbom_cfg.cmd.is_none() && sbom_cfg.args.is_none();

    if use_builtin {
        return run_sbom_builtin(ctx, dist, sbom_cfg, &project_name, &version);
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

    // GoReleaser parity (sbom.go:91-93): when artifacts != "any", multiple
    // SBOM output documents are unsupported because each document name is
    // rendered per-artifact and would clobber on collision.
    if artifacts_type != "any" && documents.len() > 1 {
        anyhow::bail!(
            "sbom[{}]: multiple SBOM outputs when artifacts={:?} is unsupported",
            id,
            artifacts_type
        );
    }

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
    let env_vars: HashMap<String, String> = sbom_cfg.env.clone().unwrap_or_else(|| {
        if cmd == "syft" && matches!(artifacts_type, "source" | "archive") {
            let mut m = HashMap::new();
            m.insert(
                "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
                "true".to_string(),
            );
            m
        } else {
            HashMap::new()
        }
    });

    // Filter artifacts from the registry based on artifacts type
    let matching_artifacts: Vec<(PathBuf, HashMap<String, String>, Option<String>)> =
        match artifacts_type {
            "any" => vec![],
            _ => {
                let kind = match artifacts_type {
                    "source" => ArtifactKind::SourceArchive,
                    "archive" => ArtifactKind::Archive,
                    "binary" => ArtifactKind::Binary,
                    "package" => ArtifactKind::LinuxPackage,
                    "diskimage" => ArtifactKind::DiskImage,
                    "installer" => ArtifactKind::Installer,
                    _ => {
                        ctx.strict_guard(
                            &log,
                            &format!(
                                "sbom[{}]: unknown artifacts type '{}', defaulting to archive",
                                id, artifacts_type
                            ),
                        )?;
                        ArtifactKind::Archive
                    }
                };

                let matched: Vec<(PathBuf, HashMap<String, String>, Option<String>)> = ctx
                    .artifacts
                    .all()
                    .iter()
                    .filter(|a| a.kind == kind)
                    .filter(|a| {
                        if let Some(ref ids) = sbom_cfg.ids {
                            if let Some(art_id) = a.metadata.get("id") {
                                ids.contains(art_id)
                            } else {
                                false
                            }
                        } else {
                            true
                        }
                    })
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
            anodize_core::template::extract_artifact_ext(artifact_name),
        );
        ctx.template_vars_mut().set(
            "ArtifactID",
            artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
        );

        if let Some(target) = artifact_target {
            let (os, arch) = anodize_core::target::map_target(target);
            ctx.template_vars_mut().set("Os", &os);
            ctx.template_vars_mut().set("Arch", &arch);
            ctx.template_vars_mut().set("Target", target);
        } else if let Some(target) = artifact_meta.get("target") {
            let (os, arch) = anodize_core::target::map_target(target);
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
        for key in &[
            "HOME",
            "USER",
            "USERPROFILE",
            "TMPDIR",
            "TMP",
            "TEMP",
            "PATH",
            "LOCALAPPDATA",
        ] {
            if let Ok(val) = std::env::var(key) {
                command.env(key, val);
            }
        }
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
            let full_path = dist.join(doc_path);
            if full_path.exists() {
                // Check the file is non-empty — a zero-byte SBOM is useless
                let file_len = std::fs::metadata(&full_path).map(|m| m.len()).unwrap_or(0);
                if file_len == 0 {
                    bail!(
                        "sbom[{}]: command succeeded but produced empty output file '{}'",
                        id,
                        doc_path
                    );
                }
                any_doc_found = true;

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
        if !any_doc_found {
            bail!(
                "sbom[{}]: command '{}' succeeded but produced no output files (expected: {:?})",
                id,
                cmd,
                rendered_docs
            );
        }
    }

    // Clear per-target template vars
    ctx.template_vars_mut().set("Os", "");
    ctx.template_vars_mut().set("Arch", "");
    ctx.template_vars_mut().set("Target", "");
    ctx.template_vars_mut().set("ArtifactName", "");
    ctx.template_vars_mut().set("ArtifactExt", "");
    ctx.template_vars_mut().set("ArtifactID", "");

    Ok(())
}

/// Built-in SBOM generation using Cargo.lock parsing (CycloneDX/SPDX).
fn run_sbom_builtin(
    ctx: &mut Context,
    dist: &Path,
    sbom_cfg: &SbomConfig,
    project_name: &str,
    version: &str,
) -> Result<()> {
    let log = ctx.logger("sbom");
    let id = sbom_cfg.id.as_deref().unwrap_or("default");

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

    let search_dir = get_repo_root()
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

    let (sbom_json, extension) = match format {
        "cyclonedx" => (
            generate_cyclonedx(project_name, version, &packages)?,
            "cdx.json",
        ),
        "spdx" => (
            generate_spdx(project_name, version, &packages)?,
            "spdx.json",
        ),
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
