use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{SbomConfig, SourceFileEntry};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Source archive generation
// ---------------------------------------------------------------------------

/// Create a source archive using `git archive`.
///
/// `git archive` automatically respects `.gitignore` and only includes
/// tracked files, which is exactly what we want for source archives.
///
/// Extra files are placed under the prefix directory (matching GoReleaser)
/// by creating a temporary staging directory and using `tar --append` to
/// insert them into the archive after creation.
fn create_source_archive(
    dist: &Path,
    format: &str,
    name: &str,
    prefix: &str,
    extra_files: &[SourceFileEntry],
    repo_root: &Path,
    commit: &str,
) -> Result<PathBuf> {
    let (git_format, extension) = match format {
        "tar.gz" | "tgz" => ("tar.gz", "tar.gz"),
        "tar" => ("tar", "tar"),
        "zip" => ("zip", "zip"),
        _ => bail!(
            "source: unsupported archive format '{}' (use tar.gz, tgz, tar, or zip)",
            format
        ),
    };

    let filename = format!("{}.{}", name, extension);
    let output_path = dist.join(&filename);

    // For tar-based formats with extra files, create as uncompressed tar first,
    // append extra files under the prefix, then compress if needed.
    let needs_post_append = !extra_files.is_empty() && git_format != "zip";
    let initial_format = if needs_post_append { "tar" } else { git_format };
    let initial_path = if needs_post_append {
        dist.join(format!("{}.tar.tmp", name))
    } else {
        output_path.clone()
    };

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd.arg("archive")
        .arg("--format")
        .arg(initial_format)
        .arg(format!("--prefix={}/", prefix))
        .arg("--output")
        .arg(&initial_path);

    // For zip format, use --add-file (files go at root — limitation accepted
    // for zip since tar append doesn't apply; zip is rare for source archives).
    if git_format == "zip" {
        for entry in extra_files {
            cmd.arg("--add-file").arg(&entry.src);
        }
    }

    cmd.arg(commit);

    let output = cmd
        .output()
        .context("source: failed to run 'git archive'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("source: git archive failed: {}", stderr.trim());
    }

    // Append extra files under the prefix directory using tar
    if needs_post_append {
        // Create a temp staging dir with the prefix structure
        let staging = dist.join(format!(".{}-extra-staging", prefix));
        let prefixed_dir = staging.join(prefix);
        std::fs::create_dir_all(&prefixed_dir).context("source: create extra files staging dir")?;

        for entry in extra_files {
            let src = Path::new(&entry.src);
            let dest_name = if let Some(ref dst) = entry.dst {
                std::ffi::OsString::from(dst)
            } else {
                src.file_name()
                    .ok_or_else(|| anyhow::anyhow!("source: extra file has no filename: {}", entry.src))?
                    .to_os_string()
            };
            std::fs::copy(src, prefixed_dir.join(&dest_name))
                .with_context(|| format!("source: copy extra file '{}' to staging", entry.src))?;
        }

        // Append to the tar archive
        let append_output = Command::new("tar")
            .arg("--append")
            .arg("-f")
            .arg(&initial_path)
            .arg("-C")
            .arg(&staging)
            .arg(prefix)
            .output()
            .context("source: failed to run 'tar --append' for extra files")?;

        if !append_output.status.success() {
            let stderr = String::from_utf8_lossy(&append_output.stderr);
            bail!("source: tar append failed: {}", stderr.trim());
        }

        // Clean up staging dir
        let _ = std::fs::remove_dir_all(&staging);

        // Compress if the original format was tar.gz
        if git_format == "tar.gz" {
            let gz_output = Command::new("gzip")
                .arg("-f")
                .arg(&initial_path)
                .output()
                .context("source: failed to gzip archive")?;

            if !gz_output.status.success() {
                let stderr = String::from_utf8_lossy(&gz_output.stderr);
                bail!("source: gzip failed: {}", stderr.trim());
            }

            // gzip creates .tar.tmp.gz — rename to final path
            let gzipped = dist.join(format!("{}.tar.tmp.gz", name));
            std::fs::rename(&gzipped, &output_path).with_context(|| {
                format!(
                    "source: rename {} -> {}",
                    gzipped.display(),
                    output_path.display()
                )
            })?;
        } else {
            // Plain tar — rename from .tar.tmp to final path
            std::fs::rename(&initial_path, &output_path).with_context(|| {
                format!(
                    "source: rename {} -> {}",
                    initial_path.display(),
                    output_path.display()
                )
            })?;
        }
    }

    Ok(output_path)
}

/// Determine the repository root via `git rev-parse --show-toplevel`.
fn get_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("source: failed to run 'git rev-parse --show-toplevel'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("source: failed to determine repo root: {}", stderr.trim());
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(root))
}

// ---------------------------------------------------------------------------
// SBOM generation
// ---------------------------------------------------------------------------

/// A parsed Cargo.lock package entry.
#[derive(Debug, Clone)]
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

    // The root package
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
///
/// Produces a deterministic hash-based identifier derived from the current
/// timestamp, process ID, and a monotonic counter. The counter ensures that
/// consecutive calls within the same nanosecond produce different values.
/// **Not cryptographically random** — suitable only for document namespaces.
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

    // Hash again with a different seed for more bits
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

// ---------------------------------------------------------------------------
// SourceStage
// ---------------------------------------------------------------------------

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

        let sbom_enabled = !ctx.config.sboms.is_empty();

        if !source_enabled && !sbom_enabled {
            log.status("nothing enabled, skipping");
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        if !ctx.is_dry_run() {
            std::fs::create_dir_all(&dist).with_context(|| {
                format!("source: failed to create dist dir: {}", dist.display())
            })?;
        }

        // --- Source archive ---
        if source_enabled {
            self.run_source_archive(ctx, &dist)?;
        }

        // --- SBOM ---
        if sbom_enabled {
            // Clone the sbom configs to avoid borrowing ctx while iterating
            let sbom_configs = ctx.config.sboms.clone();
            for sbom_cfg in &sbom_configs {
                self.run_sbom(ctx, &dist, sbom_cfg)?;
            }
        }

        Ok(())
    }
}

impl SourceStage {
    fn run_source_archive(&self, ctx: &mut Context, dist: &Path) -> Result<()> {
        let source_cfg = ctx.config.source.as_ref().unwrap();
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

        // Determine the archive prefix (directory name inside the archive)
        let prefix = if let Some(ref tpl) = source_cfg.prefix_template {
            ctx.render_template(tpl)
                .with_context(|| format!("source: failed to render prefix_template '{}'", tpl))?
        } else {
            name.clone()
        };

        let extra_files = source_cfg.files.clone();

        let log = ctx.logger("source");
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would create {}.{} archive",
                name, format
            ));
            return Ok(());
        }

        log.status(&format!("creating {}.{} archive...", name, format));

        let repo_root = get_repo_root()?;
        let commit = ctx
            .git_info
            .as_ref()
            .map(|info| info.commit.as_str())
            .unwrap_or("HEAD");
        let output_path =
            create_source_archive(dist, &format, &name, &prefix, &extra_files, &repo_root, commit)?;

        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), format);

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::SourceArchive,
            name: String::new(),
            path: output_path,
            target: None,
            crate_name: project_name.clone(),
            metadata,
        });

        Ok(())
    }

    fn run_sbom(&self, ctx: &mut Context, dist: &Path, sbom_cfg: &SbomConfig) -> Result<()> {
        let log = ctx.logger("source");
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
            return self.run_sbom_builtin(ctx, dist, sbom_cfg, &project_name, &version);
        }

        // --- External command (subprocess) model ---
        let cmd = sbom_cfg.cmd.as_deref().unwrap_or("syft");
        let artifacts_type = sbom_cfg.artifacts.as_deref().unwrap_or("archive");

        // Default documents based on artifacts type
        let documents = sbom_cfg.documents.clone().unwrap_or_else(|| {
            match artifacts_type {
                "binary" => vec!["{{ .ArtifactName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}.sbom.json".to_string()],
                "any" => vec![],
                _ => vec!["{{ .ArtifactName }}.sbom.json".to_string()],
            }
        });

        // Default args for syft
        let args = sbom_cfg.args.clone().unwrap_or_else(|| {
            if cmd == "syft" {
                vec!["$artifact".to_string(), "--output".to_string(), "spdx-json=$document".to_string()]
            } else {
                vec![]
            }
        });

        // Default env for syft with source/archive
        let env_vars = sbom_cfg.env.clone().unwrap_or_else(|| {
            if cmd == "syft" && matches!(artifacts_type, "source" | "archive") {
                vec!["SYFT_FILE_METADATA_CATALOGER_ENABLED=true".to_string()]
            } else {
                vec![]
            }
        });

        // Filter artifacts from the registry based on artifacts type
        let matching_artifacts: Vec<(PathBuf, HashMap<String, String>)> = match artifacts_type {
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

                let matched: Vec<(PathBuf, HashMap<String, String>)> = ctx
                    .artifacts
                    .all()
                    .iter()
                    .filter(|a| a.kind == kind)
                    .filter(|a| {
                        // Filter by ids if specified
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
                    .map(|a| (a.path.clone(), a.metadata.clone()))
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
                for (path, _) in &matching_artifacts {
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
        let artifact_list: Vec<(PathBuf, HashMap<String, String>)> = if artifacts_type == "any" {
            vec![(PathBuf::new(), HashMap::new())]
        } else {
            matching_artifacts
        };

        for (artifact_path, _artifact_meta) in &artifact_list {
            let artifact_rel = if artifact_path.as_os_str().is_empty() {
                String::new()
            } else {
                artifact_path
                    .strip_prefix(dist)
                    .unwrap_or(artifact_path)
                    .display()
                    .to_string()
            };

            // Render document paths
            let mut rendered_docs: Vec<String> = Vec::new();
            for doc_tpl in &documents {
                let rendered = ctx.render_template(doc_tpl)
                    .with_context(|| format!("sbom[{}]: failed to render document template '{}'", id, doc_tpl))?;
                rendered_docs.push(rendered);
            }

            let first_doc = rendered_docs.first().cloned().unwrap_or_default();

            // Render args — replace $artifact, $document, $document0, $document1, etc.
            let rendered_args: Vec<String> = args
                .iter()
                .map(|arg| {
                    let mut s = arg.replace("$artifact", &artifact_rel);
                    s = s.replace("$document", &first_doc);
                    for (i, doc) in rendered_docs.iter().enumerate() {
                        s = s.replace(&format!("$document{}", i), doc);
                    }
                    // Render template vars in args
                    ctx.render_template(&s).unwrap_or(s)
                })
                .collect();

            // Render env vars
            let rendered_env: Vec<(String, String)> = env_vars
                .iter()
                .filter_map(|e| {
                    let rendered = ctx.render_template(e).unwrap_or_else(|_| e.clone());
                    rendered.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();

            log.status(&format!(
                "sbom[{}]: running {} {}",
                id,
                cmd,
                rendered_args.join(" ")
            ));

            let mut command = Command::new(cmd);
            command.args(&rendered_args);
            command.current_dir(dist);
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
                    });
                }
            }
        }

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
            "cyclonedx" => {
                let sbom = generate_cyclonedx(project_name, version, &packages)?;
                (sbom, "cdx.json")
            }
            "spdx" => {
                let sbom = generate_spdx(project_name, version, &packages)?;
                (sbom, "spdx.json")
            }
            _ => bail!(
                "sbom[{}]: unsupported format '{}' (use cyclonedx or spdx)",
                id, format
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::test_helpers::TestContextBuilder;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Cargo.lock parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_cargo_lock_basic() {
        let content = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "anyhow"
version = "1.0.82"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "my-project"
version = "0.1.0"
"#;
        let packages = parse_cargo_lock(content).unwrap();
        assert_eq!(packages.len(), 3);

        assert_eq!(packages[0].name, "serde");
        assert_eq!(packages[0].version, "1.0.200");
        assert!(packages[0].source.is_some());
        assert!(
            packages[0]
                .source
                .as_ref()
                .unwrap()
                .starts_with("registry+")
        );

        assert_eq!(packages[1].name, "anyhow");
        assert_eq!(packages[1].version, "1.0.82");

        assert_eq!(packages[2].name, "my-project");
        assert_eq!(packages[2].version, "0.1.0");
        assert!(packages[2].source.is_none());
    }

    #[test]
    fn test_parse_cargo_lock_empty() {
        let content = "version = 4\n";
        let packages = parse_cargo_lock(content).unwrap();
        assert!(packages.is_empty());
    }

    #[test]
    fn test_parse_cargo_lock_with_dependencies() {
        let content = r#"
version = 4

[[package]]
name = "aho-corasick"
version = "1.1.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ddd31a130427c27518df266943a5308ed92d4b226cc639f5a8f1002816174301"
dependencies = [
 "memchr",
]

[[package]]
name = "memchr"
version = "2.7.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let packages = parse_cargo_lock(content).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "aho-corasick");
        assert_eq!(packages[1].name, "memchr");
    }

    #[test]
    fn test_parse_cargo_lock_invalid_toml() {
        let content = "this is not valid toml {{{{";
        let result = parse_cargo_lock(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse"));
    }

    // -----------------------------------------------------------------------
    // CycloneDX generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_cyclonedx_basic() {
        let packages = vec![
            CargoPackage {
                name: "serde".to_string(),
                version: "1.0.200".to_string(),
                source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            },
            CargoPackage {
                name: "my-lib".to_string(),
                version: "0.1.0".to_string(),
                source: None,
            },
        ];

        let sbom = generate_cyclonedx("my-project", "1.0.0", &packages).unwrap();

        // Check top-level structure
        assert_eq!(sbom["bomFormat"], "CycloneDX");
        assert_eq!(sbom["specVersion"], "1.5");
        assert_eq!(sbom["version"], 1);

        // Check metadata
        assert_eq!(sbom["metadata"]["component"]["name"], "my-project");
        assert_eq!(sbom["metadata"]["component"]["version"], "1.0.0");
        assert_eq!(sbom["metadata"]["component"]["type"], "application");
        assert!(sbom["metadata"]["timestamp"].is_string());

        // Check components
        let components = sbom["components"].as_array().unwrap();
        assert_eq!(components.len(), 2);

        assert_eq!(components[0]["name"], "serde");
        assert_eq!(components[0]["version"], "1.0.200");
        assert_eq!(components[0]["type"], "library");
        assert_eq!(components[0]["purl"], "pkg:cargo/serde@1.0.200");
        // Registry package should have externalReferences
        assert!(components[0]["externalReferences"].is_array());

        assert_eq!(components[1]["name"], "my-lib");
        assert_eq!(components[1]["version"], "0.1.0");
        // Non-registry package should not have externalReferences
        assert!(components[1]["externalReferences"].is_null());
    }

    #[test]
    fn test_generate_cyclonedx_empty_packages() {
        let sbom = generate_cyclonedx("empty-project", "0.0.1", &[]).unwrap();
        assert_eq!(sbom["bomFormat"], "CycloneDX");
        let components = sbom["components"].as_array().unwrap();
        assert!(components.is_empty());
    }

    #[test]
    fn test_generate_cyclonedx_purl_format() {
        let packages = vec![CargoPackage {
            name: "tokio".to_string(),
            version: "1.37.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        }];

        let sbom = generate_cyclonedx("test", "1.0.0", &packages).unwrap();
        let components = sbom["components"].as_array().unwrap();
        assert_eq!(components[0]["purl"], "pkg:cargo/tokio@1.37.0");
    }

    // -----------------------------------------------------------------------
    // SPDX generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_spdx_basic() {
        let packages = vec![
            CargoPackage {
                name: "serde".to_string(),
                version: "1.0.200".to_string(),
                source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            },
            CargoPackage {
                name: "local-dep".to_string(),
                version: "0.1.0".to_string(),
                source: None,
            },
        ];

        let sbom = generate_spdx("my-app", "2.0.0", &packages).unwrap();

        // Check top-level structure
        assert_eq!(sbom["spdxVersion"], "SPDX-2.3");
        assert_eq!(sbom["dataLicense"], "CC0-1.0");
        assert_eq!(sbom["SPDXID"], "SPDXRef-DOCUMENT");
        assert_eq!(sbom["name"], "my-app-2.0.0");
        assert!(
            sbom["documentNamespace"]
                .as_str()
                .unwrap()
                .starts_with("https://spdx.org/spdxdocs/my-app-2.0.0-")
        );

        // Check packages (root + 2 deps)
        let spdx_packages = sbom["packages"].as_array().unwrap();
        assert_eq!(spdx_packages.len(), 3);

        // Root package
        assert_eq!(spdx_packages[0]["SPDXID"], "SPDXRef-Package");
        assert_eq!(spdx_packages[0]["name"], "my-app");
        assert_eq!(spdx_packages[0]["versionInfo"], "2.0.0");

        // First dependency
        assert_eq!(spdx_packages[1]["SPDXID"], "SPDXRef-Package-0");
        assert_eq!(spdx_packages[1]["name"], "serde");
        assert_eq!(spdx_packages[1]["versionInfo"], "1.0.200");
        assert!(
            spdx_packages[1]["downloadLocation"]
                .as_str()
                .unwrap()
                .contains("crates.io")
        );

        // Local dependency
        assert_eq!(spdx_packages[2]["SPDXID"], "SPDXRef-Package-1");
        assert_eq!(spdx_packages[2]["name"], "local-dep");
        assert_eq!(spdx_packages[2]["downloadLocation"], "NOASSERTION");

        // Check relationships
        let relationships = sbom["relationships"].as_array().unwrap();
        // DESCRIBES + 2 DEPENDS_ON
        assert_eq!(relationships.len(), 3);
        assert_eq!(relationships[0]["relationshipType"], "DESCRIBES");
        assert_eq!(relationships[1]["relationshipType"], "DEPENDS_ON");
        assert_eq!(relationships[2]["relationshipType"], "DEPENDS_ON");
    }

    #[test]
    fn test_generate_spdx_empty_packages() {
        let sbom = generate_spdx("empty", "0.0.1", &[]).unwrap();
        assert_eq!(sbom["spdxVersion"], "SPDX-2.3");
        let spdx_packages = sbom["packages"].as_array().unwrap();
        // Only root package
        assert_eq!(spdx_packages.len(), 1);
        let relationships = sbom["relationships"].as_array().unwrap();
        // Only DESCRIBES
        assert_eq!(relationships.len(), 1);
    }

    #[test]
    fn test_generate_spdx_purl_in_external_refs() {
        let packages = vec![CargoPackage {
            name: "clap".to_string(),
            version: "4.5.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        }];

        let sbom = generate_spdx("test", "1.0.0", &packages).unwrap();
        let spdx_packages = sbom["packages"].as_array().unwrap();
        let dep = &spdx_packages[1];
        let ext_refs = dep["externalRefs"].as_array().unwrap();
        assert_eq!(ext_refs[0]["referenceCategory"], "PACKAGE-MANAGER");
        assert_eq!(ext_refs[0]["referenceType"], "purl");
        assert_eq!(ext_refs[0]["referenceLocator"], "pkg:cargo/clap@4.5.0");
    }

    // -----------------------------------------------------------------------
    // Config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_source_config_defaults() {
        use anodize_core::config::SourceConfig;
        let cfg = SourceConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.archive_format(), "tar.gz");
    }

    #[test]
    fn test_source_config_enabled() {
        use anodize_core::config::{SourceConfig, SourceFileEntry};
        let cfg = SourceConfig {
            enabled: Some(true),
            format: Some("zip".to_string()),
            name_template: Some("{{ .ProjectName }}-src-{{ .Version }}".to_string()),
            prefix_template: None,
            files: vec![SourceFileEntry {
                src: "LICENSE".to_string(),
                ..Default::default()
            }],
        };
        assert!(cfg.is_enabled());
        assert_eq!(cfg.archive_format(), "zip");
    }

    #[test]
    fn test_sbom_config_defaults() {
        use anodize_core::config::SbomConfig;
        let cfg = SbomConfig::default();
        // All fields are None by default
        assert!(cfg.cmd.is_none());
        assert!(cfg.artifacts.is_none());
        assert!(cfg.disable.is_none());
    }

    #[test]
    fn test_config_with_source_and_sbom_yaml() {
        let yaml = r#"
project_name: my-app
crates: []
source:
  enabled: true
  format: tar.gz
  name_template: "{{ .ProjectName }}-source-{{ .Version }}"
sbom:
  cmd: syft
  artifacts: archive
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.source.is_some());
        let source = config.source.as_ref().unwrap();
        assert!(source.is_enabled());
        assert_eq!(source.archive_format(), "tar.gz");
        assert!(source.name_template.is_some());

        assert_eq!(config.sboms.len(), 1);
        let sbom = &config.sboms[0];
        assert_eq!(sbom.cmd.as_deref(), Some("syft"));
        assert_eq!(sbom.artifacts.as_deref(), Some("archive"));
    }

    #[test]
    fn test_config_without_source_and_sbom() {
        let yaml = r#"
project_name: minimal
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.source.is_none());
        assert!(config.sboms.is_empty());
    }

    // -----------------------------------------------------------------------
    // Source archive stage (integration-style)
    // -----------------------------------------------------------------------

    #[test]
    fn test_source_archive_with_git_repo() {
        use anodize_core::test_helpers::{create_test_project, init_git_repo};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create a test project and git repo
        create_test_project(tmp.path());
        init_git_repo(tmp.path());

        // First create dist dir
        std::fs::create_dir_all(&dist).unwrap();

        let output = std::process::Command::new("git")
            .args([
                "archive",
                "--format",
                "tar.gz",
                "--prefix",
                "test-project-1.2.3/",
                "--output",
            ])
            .arg(dist.join("test-project-1.2.3.tar.gz").to_str().unwrap())
            .arg("HEAD")
            .current_dir(tmp.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "git archive failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let archive_path = dist.join("test-project-1.2.3.tar.gz");
        assert!(archive_path.exists());
        assert!(std::fs::metadata(&archive_path).unwrap().len() > 0);
    }

    #[test]
    fn test_source_archive_zip_format_with_git_repo() {
        use anodize_core::test_helpers::{create_test_project, init_git_repo};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        create_test_project(tmp.path());
        init_git_repo(tmp.path());

        let output = std::process::Command::new("git")
            .args([
                "archive",
                "--format",
                "zip",
                "--prefix",
                "test-project-1.2.3/",
                "--output",
            ])
            .arg(dist.join("test-project-1.2.3.zip").to_str().unwrap())
            .arg("HEAD")
            .current_dir(tmp.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "git archive failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let archive_path = dist.join("test-project-1.2.3.zip");
        assert!(archive_path.exists());
        assert!(std::fs::metadata(&archive_path).unwrap().len() > 0);
    }

    // -----------------------------------------------------------------------
    // SBOM stage (integration-style using actual Cargo.lock)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sbom_from_real_cargo_lock() {
        let content = r#"
version = 4

[[package]]
name = "anyhow"
version = "1.0.82"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc123"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "def456"

[[package]]
name = "my-app"
version = "0.1.0"
dependencies = [
 "anyhow",
 "serde",
]
"#;

        let packages = parse_cargo_lock(content).unwrap();
        assert_eq!(packages.len(), 3);

        // Test CycloneDX generation from these packages
        let cdx = generate_cyclonedx("my-app", "0.1.0", &packages).unwrap();
        let cdx_str = serde_json::to_string_pretty(&cdx).unwrap();
        assert!(cdx_str.contains("CycloneDX"));
        assert!(cdx_str.contains("anyhow"));
        assert!(cdx_str.contains("serde"));

        // Test SPDX generation from these packages
        let spdx = generate_spdx("my-app", "0.1.0", &packages).unwrap();
        let spdx_str = serde_json::to_string_pretty(&spdx).unwrap();
        assert!(spdx_str.contains("SPDX-2.3"));
        assert!(spdx_str.contains("anyhow"));
        assert!(spdx_str.contains("serde"));
    }

    #[test]
    fn test_sbom_written_to_file() {
        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let packages = vec![CargoPackage {
            name: "tokio".to_string(),
            version: "1.37.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        }];

        // CycloneDX
        let cdx = generate_cyclonedx("my-app", "1.0.0", &packages).unwrap();
        let cdx_path = dist.join("my-app-1.0.0.cdx.json");
        let json_str = serde_json::to_string_pretty(&cdx).unwrap();
        std::fs::write(&cdx_path, &json_str).unwrap();
        assert!(cdx_path.exists());

        // Read it back and verify
        let read_back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cdx_path).unwrap()).unwrap();
        assert_eq!(read_back["bomFormat"], "CycloneDX");

        // SPDX
        let spdx = generate_spdx("my-app", "1.0.0", &packages).unwrap();
        let spdx_path = dist.join("my-app-1.0.0.spdx.json");
        let json_str = serde_json::to_string_pretty(&spdx).unwrap();
        std::fs::write(&spdx_path, &json_str).unwrap();
        assert!(spdx_path.exists());

        let read_back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&spdx_path).unwrap()).unwrap();
        assert_eq!(read_back["spdxVersion"], "SPDX-2.3");
    }

    // -----------------------------------------------------------------------
    // Dry-run behavior
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_dry_run_does_not_create_files() {
        use anodize_core::config::{SbomConfig, SourceConfig};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let mut ctx = TestContextBuilder::new()
            .project_name("test-app")
            .dry_run(true)
            .dist(dist.clone())
            .build();

        ctx.config.source = Some(SourceConfig {
            enabled: Some(true),
            format: Some("tar.gz".to_string()),
            name_template: None,
            prefix_template: None,
            files: vec![],
        });
        ctx.config.sboms = vec![SbomConfig {
            ..Default::default()
        }];

        let stage = SourceStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok(), "dry-run should succeed: {:?}", result.err());

        // Dist dir should not be created in dry-run mode
        assert!(!dist.exists(), "dist dir should not be created in dry-run");
        assert_eq!(
            ctx.artifacts.all().len(),
            0,
            "no artifacts should be registered in dry-run"
        );
    }

    #[test]
    fn test_stage_skips_when_nothing_enabled() {
        let mut ctx = TestContextBuilder::new().build();
        // No source or sbom config at all
        ctx.config.source = None;
        ctx.config.sboms = vec![];

        let stage = SourceStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
        assert_eq!(ctx.artifacts.all().len(), 0);
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodize_core::config::SourceConfig;

        let mut ctx = TestContextBuilder::new().build();
        ctx.config.source = Some(SourceConfig {
            enabled: Some(false),
            ..Default::default()
        });
        // Empty sboms vec means no SBOM generation
        ctx.config.sboms = vec![];

        let stage = SourceStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
        assert_eq!(ctx.artifacts.all().len(), 0);
    }

    // -----------------------------------------------------------------------
    // ArtifactKind variants
    // -----------------------------------------------------------------------

    #[test]
    fn test_artifact_kind_source_archive() {
        assert_eq!(ArtifactKind::SourceArchive.as_str(), "source_archive");
        let json = serde_json::to_value(ArtifactKind::SourceArchive).unwrap();
        assert_eq!(json, "source_archive");
    }

    #[test]
    fn test_artifact_kind_sbom() {
        assert_eq!(ArtifactKind::Sbom.as_str(), "sbom");
        let json = serde_json::to_value(ArtifactKind::Sbom).unwrap();
        assert_eq!(json, "sbom");
    }

    // -----------------------------------------------------------------------
    // UUID generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_uuid_v4_simple_format() {
        let uuid = uuid_v4_simple();
        // Should be in format: 8-4-4-4-12 hex chars
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(parts.len(), 5, "UUID should have 5 parts: {}", uuid);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);

        // Version nibble should be 4
        assert!(
            parts[2].starts_with('4'),
            "UUID version nibble should be 4: {}",
            uuid
        );
    }

    // -----------------------------------------------------------------------
    // SBOM format validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cyclonedx_has_required_fields() {
        let packages = vec![CargoPackage {
            name: "test-dep".to_string(),
            version: "1.0.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        }];

        let sbom = generate_cyclonedx("proj", "1.0.0", &packages).unwrap();

        // Required CycloneDX 1.5 fields
        assert!(sbom.get("bomFormat").is_some(), "missing bomFormat");
        assert!(sbom.get("specVersion").is_some(), "missing specVersion");
        assert!(sbom.get("version").is_some(), "missing version");
        assert!(sbom.get("metadata").is_some(), "missing metadata");
        assert!(sbom.get("components").is_some(), "missing components");

        // Metadata sub-fields
        let metadata = &sbom["metadata"];
        assert!(metadata.get("timestamp").is_some(), "missing timestamp");
        assert!(metadata.get("component").is_some(), "missing component");
        assert!(metadata.get("tools").is_some(), "missing tools");

        // Component sub-fields
        let comp = &sbom["components"][0];
        assert!(comp.get("type").is_some(), "missing component type");
        assert!(comp.get("name").is_some(), "missing component name");
        assert!(comp.get("version").is_some(), "missing component version");
        assert!(comp.get("purl").is_some(), "missing component purl");
    }

    #[test]
    fn test_spdx_has_required_fields() {
        let packages = vec![CargoPackage {
            name: "test-dep".to_string(),
            version: "1.0.0".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        }];

        let sbom = generate_spdx("proj", "1.0.0", &packages).unwrap();

        // Required SPDX 2.3 fields
        assert!(sbom.get("spdxVersion").is_some(), "missing spdxVersion");
        assert!(sbom.get("dataLicense").is_some(), "missing dataLicense");
        assert!(sbom.get("SPDXID").is_some(), "missing SPDXID");
        assert!(sbom.get("name").is_some(), "missing name");
        assert!(
            sbom.get("documentNamespace").is_some(),
            "missing documentNamespace"
        );
        assert!(sbom.get("creationInfo").is_some(), "missing creationInfo");
        assert!(sbom.get("packages").is_some(), "missing packages");
        assert!(sbom.get("relationships").is_some(), "missing relationships");

        // Package sub-fields
        let pkg = &sbom["packages"][1]; // first dependency (index 0 is root)
        assert!(pkg.get("SPDXID").is_some(), "missing package SPDXID");
        assert!(pkg.get("name").is_some(), "missing package name");
        assert!(
            pkg.get("versionInfo").is_some(),
            "missing package versionInfo"
        );
        assert!(
            pkg.get("downloadLocation").is_some(),
            "missing package downloadLocation"
        );
        assert!(
            pkg.get("externalRefs").is_some(),
            "missing package externalRefs"
        );
    }

    // -----------------------------------------------------------------------
    // SourceStage integration test (runs through the Stage interface)
    // -----------------------------------------------------------------------

    #[test]
    fn test_source_stage_run_creates_archive_in_git_repo() {
        use anodize_core::config::SourceConfig;
        use anodize_core::stage::Stage;
        use anodize_core::test_helpers::{create_test_project, init_git_repo};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create a test project and git repo
        create_test_project(tmp.path());
        // Write a Cargo.lock so SBOM can also find it (not needed for this test
        // but keeps the fixture realistic)
        std::fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        init_git_repo(tmp.path());

        // Get the real commit hash from the test repo so git archive can resolve it
        let real_commit = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(tmp.path())
            .output()
            .expect("git rev-parse HEAD should succeed");
        let real_commit = String::from_utf8_lossy(&real_commit.stdout)
            .trim()
            .to_string();

        let mut ctx = TestContextBuilder::new()
            .project_name("test-project")
            .commit(&real_commit)
            .source(SourceConfig {
                enabled: Some(true),
                format: Some("tar.gz".to_string()),
                name_template: None,
                prefix_template: None,
                files: vec![],
            })
            .dist(dist.clone())
            .build();

        // Run from the temp dir so git commands find the repo
        let orig_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let stage = SourceStage;
        let result = stage.run(&mut ctx);

        // Restore CWD before asserting (so failures don't leave CWD wrong)
        std::env::set_current_dir(&orig_dir).unwrap();

        assert!(
            result.is_ok(),
            "SourceStage.run() should succeed: {:?}",
            result.err()
        );

        // Should have produced exactly one source archive artifact
        let artifacts = ctx.artifacts.all();
        assert_eq!(
            artifacts.len(),
            1,
            "expected 1 artifact, got {}",
            artifacts.len()
        );
        assert_eq!(artifacts[0].kind, ArtifactKind::SourceArchive);
        assert!(
            artifacts[0].path.exists(),
            "archive file should exist at {:?}",
            artifacts[0].path
        );
        assert!(
            std::fs::metadata(&artifacts[0].path).unwrap().len() > 0,
            "archive file should not be empty"
        );
    }
}
