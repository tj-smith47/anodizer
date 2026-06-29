//! SBOM (Software Bill of Materials) generation stage for anodizer.
//!
//! Supports two modes:
//! 1. **Built-in**: Parses `Cargo.lock` to generate CycloneDX 1.5 or SPDX 2.3 JSON.
//!    This is a Rust-specific value-add.
//! 2. **External command**: Runs an external tool (default: `syft`) to catalog artifacts.
//!    Standard SBOM-generation behavior.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::config::SbomConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod expected;

pub use expected::expected_sbom_assets;

/// One artifact an SBOM config selected: path, metadata, build target, and
/// kind. The kind is `None` only for the synthetic whole-project subject of
/// `artifacts: any` (which catalogs the source tree, not one artifact).
type SbomSubject = (
    PathBuf,
    HashMap<String, String>,
    Option<String>,
    Option<ArtifactKind>,
);

/// Map a typed (non-`any`, non-`binary`) `artifacts:` filter value to the
/// artifact kind it selects. Shared by both generation modes and the
/// expected-asset derivation so the selection cannot drift between them.
pub(crate) fn typed_artifact_kind(artifacts_type: &str, id: &str) -> Result<ArtifactKind> {
    match artifacts_type {
        "source" => Ok(ArtifactKind::SourceArchive),
        "archive" => Ok(ArtifactKind::Archive),
        "package" => Ok(ArtifactKind::LinuxPackage),
        "diskimage" => Ok(ArtifactKind::DiskImage),
        "installer" => Ok(ArtifactKind::Installer),
        other => bail!(
            "sbom[{}]: unknown artifacts type '{}'. Valid values are: \
             source, archive, package, diskimage, installer, binary, any",
            id,
            other
        ),
    }
}

/// Build the per-artifact template-variable overlay used to render SBOM
/// `documents:` / `args:` / `env:` templates (`ArtifactName`, `ArtifactExt`,
/// `ArtifactID`, and `Os`/`Arch`/`Target` when the artifact has a build
/// target).
///
/// Returns a CLONE of the context's vars with the bindings applied — the
/// shared context is never mutated, so one artifact's `Os`/`Arch`/`Target`
/// cannot leak into the next artifact (or into downstream stages). Shared by
/// both generation modes and the expected-asset derivation so all three
/// render with identical bindings.
pub(crate) fn artifact_template_vars(
    ctx: &Context,
    artifact_path: &Path,
    artifact_meta: &HashMap<String, String>,
    artifact_target: Option<&str>,
) -> anodizer_core::template::TemplateVars {
    let artifact_name = artifact_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let mut vars = ctx.template_vars().clone();
    vars.set("ArtifactName", artifact_name);
    vars.set(
        "ArtifactExt",
        artifact_meta
            .get("ext")
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .unwrap_or_else(|| anodizer_core::template::extract_artifact_ext(artifact_name)),
    );
    vars.set(
        "ArtifactID",
        artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
    );
    let target = artifact_target.or_else(|| artifact_meta.get("target").map(String::as_str));
    if let Some(target) = target {
        let (os, arch) = anodizer_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    }
    vars
}

/// Warn when a configured `ids:` filter is the reason an SBOM config matched
/// nothing — a typo'd build id would otherwise silently no-op the config.
fn warn_ids_eliminated_all(
    log: &anodizer_core::log::StageLogger,
    id: &str,
    ids: Option<&[String]>,
    pre_filter: usize,
    post_filter: usize,
) {
    if anodizer_core::artifact::ids_filter_eliminated_all(ids, pre_filter, post_filter) {
        log.warn(&format!(
            "ids filter {:?} on sbom[{}] matched no artifacts — this config will \
             produce NO SBOMs",
            ids.unwrap_or(&[]),
            id
        ));
    }
}

/// Detect the built-in SBOM format (and its file extension) from the
/// `documents:` templates' trailing extension chain.
/// `mytool-spdx-companion.cdx.json` resolves to CycloneDX because the
/// trailing extension is `.cdx.json`; a raw substring match on the marketing
/// word in the basename would flip to SPDX and produce a
/// CycloneDX-by-name / SPDX-by-payload file.
pub(crate) fn builtin_format_and_extension(documents: &[String]) -> (&'static str, &'static str) {
    let mut detected = ("cyclonedx", "cdx.json");
    for d in documents {
        let lower = d.to_lowercase();
        if lower.ends_with(".spdx.json") || lower.ends_with(".spdx") {
            detected = ("spdx", "spdx.json");
            break;
        }
        if lower.ends_with(".cdx.json") || lower.ends_with(".cyclonedx.json") {
            detected = ("cyclonedx", "cdx.json");
            break;
        }
    }
    detected
}

// ---------------------------------------------------------------------------
// Built-in SBOM generation (Rust-specific)
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
    log.debug("running git rev-parse --show-toplevel");
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

    /// Regression:
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

    // -----------------------------------------------------------------------
    // External-command (subprocess) path — driven by the fake-tool harness.
    // -----------------------------------------------------------------------
    //
    // The `cmd:` field is configurable, so each test points it straight at a
    // stub installed via `FakeToolDir` (no PATH mutation, no `#[serial]`). The
    // stub records its argv (`tools.calls`), can create output files
    // (`.creates`), exit non-zero (`.exit`/`.stderr`), or run an arbitrary
    // `sh` body (`.script`) so env propagation and per-arg syft semantics are
    // observable.

    #[cfg(unix)]
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    #[cfg(unix)]
    use std::collections::HashMap;

    /// Build a `Context` with `dist` set to a fresh tempdir and a single SBOM
    /// config pointed at `cmd`. Returns `(ctx, tmpdir)`; the tmpdir guard must
    /// outlive the stage run.
    #[cfg(unix)]
    fn external_ctx(cmd: PathBuf, cfg: SbomConfig) -> (Context, tempfile::TempDir) {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let dist = tmpdir.path().to_path_buf();
        let cfg = SbomConfig {
            cmd: Some(cmd.to_string_lossy().into_owned()),
            ..cfg
        };
        let ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(dist)
            .add_sbom(cfg)
            .build();
        (ctx, tmpdir)
    }

    /// Register a Binary artifact in `dist` so `artifacts: binary` configs have
    /// something to catalog. Returns the on-disk binary path.
    #[cfg(unix)]
    fn add_binary(ctx: &mut Context, dist: &Path, name: &str, target: &str) -> PathBuf {
        let path = dist.join(name);
        std::fs::write(&path, b"\x7fELF fake").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: name.to_string(),
            path: path.clone(),
            target: Some(target.to_string()),
            crate_name: "myproj".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        path
    }

    /// Happy path: the stage shells out to the configured tool, the tool writes
    /// the rendered document, and that file is registered as an Sbom artifact.
    /// Asserts the tool was invoked exactly once with the rendered argv (the
    /// `$document`/`$artifact` placeholders resolved).
    #[cfg(unix)]
    #[test]
    fn external_cmd_success_registers_output() {
        let tools = FakeToolDir::new();
        // syft-style: write the path named in the `spdx-json=PATH` arg.
        tools
            .tool("syft")
            .script(
                "for a in \"$@\"; do case \"$a\" in *=*) echo '{\"k\":1}' > \"${a#*=}\";; esac; done",
            )
            .install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("syftcfg".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec![
                    "scan".into(),
                    "--output".into(),
                    "spdx-json=$document".into(),
                ]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        SbomStage.run(&mut ctx).expect("sbom stage");

        // Tool invoked once with the rendered argv ($document -> bom.spdx.json).
        assert_eq!(tools.call_count("syft"), 1);
        assert_eq!(
            tools.calls("syft")[0],
            vec!["scan", "--output", "spdx-json=bom.spdx.json"]
        );

        // The produced file is registered as an Sbom artifact, basename = file.
        let sboms: Vec<&Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .collect();
        assert_eq!(sboms.len(), 1);
        assert_eq!(sboms[0].name, "bom.spdx.json");
        assert_eq!(
            sboms[0].metadata.get("sbom_id").map(String::as_str),
            Some("syftcfg")
        );
        assert!(sboms[0].path.exists());
    }

    /// A macOS `.app` bundle (Installer + format=appbundle, a directory) must be
    /// excluded from `artifacts: installer` SBOM generation — it is never an
    /// uploaded asset, so cataloging it produces a stray SBOM. The sibling
    /// Installer FILE (an MSI) is still cataloged: the tool runs exactly once.
    #[cfg(unix)]
    #[test]
    fn external_installer_excludes_appbundle_directory() {
        use anodizer_core::artifact::{FORMAT_APPBUNDLE, FORMAT_META};

        let tools = FakeToolDir::new();
        tools
            .tool("syft")
            .script(
                "for a in \"$@\"; do case \"$a\" in *=*) echo '{\"k\":1}' > \"${a#*=}\";; esac; done",
            )
            .install();

        let (mut ctx, tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("inst".into()),
                artifacts: Some("installer".into()),
                documents: Some(vec!["{{ .ArtifactName }}.cdx.json".into()]),
                args: Some(vec![
                    "scan".into(),
                    "--output".into(),
                    "cyclonedx-json=$document".into(),
                ]),
                env: Some(vec![]),
                ..Default::default()
            },
        );
        let dist = tmp.path().to_path_buf();

        // The `.app` bundle: a DIRECTORY, registered Installer + format=appbundle.
        let app_dir = dist.join("MyApp.app");
        std::fs::create_dir_all(&app_dir).unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: "MyApp.app".to_string(),
            path: app_dir,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myproj".to_string(),
            metadata: HashMap::from([(FORMAT_META.to_string(), FORMAT_APPBUNDLE.to_string())]),
            size: None,
        });
        // The sibling Installer FILE (MSI): must still be cataloged.
        let msi = dist.join("myproj-1.0.0.msi");
        std::fs::write(&msi, b"MSI fake").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: "myproj-1.0.0.msi".to_string(),
            path: msi,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myproj".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        SbomStage.run(&mut ctx).expect("sbom stage");

        // Tool ran exactly once — for the MSI file, never the `.app` directory.
        assert_eq!(
            tools.call_count("syft"),
            1,
            "syft must catalog only the MSI file, not the `.app` directory"
        );
        let sboms: Vec<&Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .collect();
        assert_eq!(sboms.len(), 1, "exactly one SBOM, for the MSI");
        assert!(
            sboms[0].name.contains("myproj-1.0.0.msi"),
            "the SBOM is for the MSI, got: {}",
            sboms[0].name
        );
    }

    /// Tool exits non-zero → the stage bails and the error chain carries the
    /// tool's trimmed stderr plus the `sbom[<id>]` prefix.
    #[cfg(unix)]
    #[test]
    fn external_cmd_nonzero_exit_bails_with_stderr() {
        let tools = FakeToolDir::new();
        tools
            .tool("syft")
            .stderr("catalog failed: boom\n")
            .exit(3)
            .install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("failer".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("non-zero exit must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[failer]"), "got: {chain}");
        assert!(chain.contains("failed"), "got: {chain}");
        assert!(chain.contains("catalog failed: boom"), "got: {chain}");
        // No artifact registered on failure.
        assert!(
            ctx.artifacts
                .all()
                .iter()
                .all(|a| a.kind != ArtifactKind::Sbom)
        );
    }

    /// Tool succeeds but writes a zero-byte document → the stage rejects the
    /// empty SBOM rather than registering a useless file.
    #[cfg(unix)]
    #[test]
    fn external_cmd_empty_output_file_bails() {
        let tools = FakeToolDir::new();
        // Exit 0 but create the document as an empty file.
        tools.tool("syft").script("> bom.spdx.json").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("empty".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage.run(&mut ctx).expect_err("empty output must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[empty]"), "got: {chain}");
        assert!(chain.contains("empty output file"), "got: {chain}");
    }

    /// Tool succeeds (exit 0) but produces NO output files → the stage bails
    /// listing the expected document paths.
    #[cfg(unix)]
    #[test]
    fn external_cmd_no_output_files_bails() {
        let tools = FakeToolDir::new();
        // Exit 0 and create nothing.
        tools.tool("syft").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("noout".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("missing output must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[noout]"), "got: {chain}");
        assert!(chain.contains("no output files"), "got: {chain}");
        assert!(chain.contains("bom.spdx.json"), "got: {chain}");
        // The tool was actually run (this is the post-success check, not a
        // pre-flight skip).
        assert_eq!(tools.call_count("syft"), 1);
    }

    /// A rendered document path that resolves to an absolute path is refused —
    /// SBOM outputs must stay relative to `dist`. The tool must NOT be invoked
    /// (the bail happens during doc rendering, before the spawn).
    #[cfg(unix)]
    #[test]
    fn external_cmd_absolute_document_path_bails() {
        let tools = FakeToolDir::new();
        tools.tool("syft").creates("ignored", "x").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("abs".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["/etc/escape.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("absolute document path must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[abs]"), "got: {chain}");
        assert!(chain.contains("is absolute"), "got: {chain}");
        assert!(
            !tools.was_called("syft"),
            "tool must not run when the document path is rejected"
        );
    }

    /// `artifacts: binary` with more than one default document is a config the
    /// stage rejects up front (per-artifact document names would collide). The
    /// tool is never invoked.
    #[cfg(unix)]
    #[test]
    fn external_cmd_multiple_documents_with_typed_artifacts_bails() {
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("multi".into()),
                artifacts: Some("binary".into()),
                documents: Some(vec!["a.spdx.json".into(), "b.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("multi-document + typed artifacts must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[multi]"), "got: {chain}");
        assert!(chain.contains("multiple SBOM outputs"), "got: {chain}");
        assert!(chain.contains("binary"), "got: {chain}");
        assert!(!tools.was_called("syft"));
    }

    /// Explicit `env:` entries are template-rendered and passed to the
    /// subprocess. The stub dumps a chosen env var into a file so the test can
    /// read back the value the stage actually exported (incl. `{{ .Version }}`
    /// rendering).
    #[cfg(unix)]
    #[test]
    fn external_cmd_renders_and_passes_env() {
        let tools = FakeToolDir::new();
        // Record the env var into a file, then write the document so the stage
        // doesn't bail on a missing output.
        tools
            .tool("syft")
            .script(
                "printf '%s' \"$SBOM_PROBE\" > env_probe.txt\n\
                 echo '{}' > bom.spdx.json",
            )
            .install();

        let (mut ctx, tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("envcfg".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                // Value is a template — must be rendered to "v=1.0.0".
                env: Some(vec!["SBOM_PROBE=v={{ .Version }}".into()]),
                ..Default::default()
            },
        );

        SbomStage.run(&mut ctx).expect("sbom stage");

        let probe = std::fs::read_to_string(tmp.path().join("env_probe.txt")).unwrap();
        assert_eq!(
            probe, "v=1.0.0",
            "env value must be template-rendered and exported to the subprocess"
        );
    }

    /// `default_syft_env_for` true branch: a literal `syft` cmd with
    /// `artifacts: archive` (or `source`) injects the file-metadata cataloger
    /// env; every other combination is empty. Driven directly because the stage
    /// always resolves `cmd` to an absolute stub path, which is never the
    /// literal string `"syft"`.
    #[test]
    fn default_syft_env_true_branch_and_negatives() {
        assert_eq!(
            anodizer_core::config::SbomConfig::default_syft_env_for("syft", "archive"),
            vec![(
                "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
                "true".to_string()
            )]
        );
        assert_eq!(
            anodizer_core::config::SbomConfig::default_syft_env_for("syft", "source"),
            vec![(
                "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
                "true".to_string()
            )]
        );
        // binary/any => no special env even for syft.
        assert!(
            anodizer_core::config::SbomConfig::default_syft_env_for("syft", "binary").is_empty()
        );
        assert!(anodizer_core::config::SbomConfig::default_syft_env_for("syft", "any").is_empty());
        // non-syft cmd => never injected.
        assert!(
            anodizer_core::config::SbomConfig::default_syft_env_for("trivy", "archive").is_empty()
        );
    }

    /// Via the stage: because the resolved cmd is an absolute path (never the
    /// literal `"syft"`), the default syft env is NOT injected when `env:` is
    /// unset — the subprocess sees an empty `SYFT_FILE_METADATA_CATALOGER_ENABLED`.
    /// Pins the None-env → `default_syft_env_for` resolution path end-to-end.
    #[cfg(unix)]
    #[test]
    fn external_cmd_absolute_cmd_does_not_inject_default_syft_env() {
        let tools = FakeToolDir::new();
        // The configured cmd must be NAMED `syft` for default_syft_env_for to
        // fire, so install the stub under that name and point cmd at it.
        tools
            .tool("syft")
            .script(
                "printf '%s' \"$SYFT_FILE_METADATA_CATALOGER_ENABLED\" > env_probe.txt\n\
                 echo '{}' > arch.spdx.json",
            )
            .install();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let dist = tmpdir.path().to_path_buf();
        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(dist.clone())
            .add_sbom(SbomConfig {
                id: Some("archcfg".into()),
                // cmd basename is "syft" → default_syft_env_for triggers.
                cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
                artifacts: Some("archive".into()),
                documents: Some(vec!["arch.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                // env unset → falls to default_syft_env_for(cmd, "archive").
                ..Default::default()
            })
            .build();

        // Provide one Archive artifact so the `archive` filter matches.
        let arch_path = dist.join("pkg.tar.gz");
        std::fs::write(&arch_path, b"archive").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "pkg.tar.gz".into(),
            path: arch_path,
            target: Some("x86_64-unknown-linux-gnu".into()),
            crate_name: "myproj".into(),
            metadata: HashMap::new(),
            size: None,
        });

        SbomStage.run(&mut ctx).expect("sbom stage");

        // default_syft_env_for keys on the resolved cmd string. The config cmd
        // is an absolute path whose basename is `syft`, but resolved_cmd()
        // returns the full path — so the default env only fires when the cmd
        // string equals "syft". Assert the actual exported value to pin which
        // branch ran.
        let probe = std::fs::read_to_string(tmpdir.path().join("env_probe.txt")).unwrap();
        assert_eq!(
            probe, "",
            "an absolute cmd path is not the literal \"syft\", so the default \
             syft env must NOT be injected (resolved_cmd compares the full string)"
        );
    }

    /// `artifacts: binary` catalogs each matched binary: the rendered
    /// `$artifact` placeholder is the binary's path relative to dist, and the
    /// per-artifact `$document` is written + registered. Pins the typed-artifact
    /// iteration + `$artifact`/`$document` substitution in the external path.
    #[cfg(unix)]
    #[test]
    fn external_cmd_binary_artifacts_substitutes_artifact_and_document() {
        let tools = FakeToolDir::new();
        tools
            .tool("syft")
            .script("for a in \"$@\"; do case \"$a\" in *=*) echo '{}' > \"${a#*=}\";; esac; done")
            .install();

        let (mut ctx, tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("bin".into()),
                artifacts: Some("binary".into()),
                documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
                args: Some(vec![
                    "scan".into(),
                    "$artifact".into(),
                    "--output".into(),
                    "spdx-json=$document".into(),
                ]),
                env: Some(vec![]),
                ..Default::default()
            },
        );
        let dist = tmp.path().to_path_buf();
        add_binary(&mut ctx, &dist, "myproj-linux", "x86_64-unknown-linux-gnu");

        SbomStage.run(&mut ctx).expect("sbom stage");

        let call = &tools.calls("syft")[0];
        // $artifact -> the binary path relative to dist; $document -> rendered
        // per-artifact name.
        assert_eq!(
            call,
            &vec![
                "scan",
                "myproj-linux",
                "--output",
                "spdx-json=myproj-linux.spdx.json",
            ]
        );
        let sbom_names: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(sbom_names, vec!["myproj-linux.spdx.json"]);
    }

    /// `artifacts: archive` matching zero artifacts in non-strict mode is a
    /// silent skip: no error, no tool run, no SBOM registered.
    #[cfg(unix)]
    #[test]
    fn external_cmd_no_matching_artifacts_non_strict_skips() {
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("nomatch".into()),
                artifacts: Some("archive".into()),
                documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );
        // No Archive artifacts registered.

        SbomStage
            .run(&mut ctx)
            .expect("zero matches under non-strict must skip, not error");
        assert!(
            !tools.was_called("syft"),
            "tool must not run with no inputs"
        );
        assert!(
            ctx.artifacts
                .all()
                .iter()
                .all(|a| a.kind != ArtifactKind::Sbom)
        );
    }

    /// Built-in (Cargo.lock) generator with `artifacts: archive` and MULTIPLE
    /// archives emits exactly ONE workspace SBOM, not one byte-identical copy
    /// per archive. The built-in output is archive-independent (it catalogs the
    /// dependency graph), so N copies would only multiply the downstream
    /// checksum + signature object count for no information gain.
    #[cfg(unix)]
    #[test]
    fn builtin_archive_emits_single_workspace_sbom() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let dist = tmpdir.path().to_path_buf();
        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(dist.clone())
            // No `cmd`/`args` → use_builtin path. `documents:` provides only the
            // format/extension hint (.cdx.json → cyclonedx); the built-in path
            // does not render it per-archive.
            .add_sbom(SbomConfig {
                id: Some("wsbom".into()),
                artifacts: Some("archive".into()),
                documents: Some(vec!["{{ .ArtifactName }}.cdx.json".into()]),
                ..Default::default()
            })
            .build();

        // Three Archive artifacts across distinct targets — the pre-fix loop
        // would write three identical `.cdx.json` files.
        for (name, target) in [
            ("pkg-linux.tar.gz", "x86_64-unknown-linux-gnu"),
            ("pkg-mac.tar.gz", "aarch64-apple-darwin"),
            ("pkg-win.zip", "x86_64-pc-windows-msvc"),
        ] {
            let p = dist.join(name);
            std::fs::write(&p, b"archive").unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                name: name.into(),
                path: p,
                target: Some(target.into()),
                crate_name: "myproj".into(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        SbomStage.run(&mut ctx).expect("built-in sbom stage");

        let sboms: Vec<&Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .collect();
        assert_eq!(
            sboms.len(),
            1,
            "built-in generator + artifacts:archive must emit exactly ONE \
             workspace SBOM, not one per archive; got {:?}",
            sboms.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        // The single SBOM is the workspace-level `<project>-<version>.<ext>`
        // document, not a per-archive filename.
        assert_eq!(sboms[0].name, "myproj-1.0.0.cdx.json");
        assert!(sboms[0].path.exists(), "the workspace SBOM must be on disk");
        // Target-independent: it must NOT inherit the first matched archive's
        // target. A per-archive target makes each shard of a sharded release
        // stamp this byte-identical document with a different target, defeating
        // the per-shard merge's `dedupe_targetless_duplicates` and tripping its
        // duplicate-path guard.
        assert_eq!(
            sboms[0].target, None,
            "workspace SBOM must be target-independent so cross-shard merge \
             collapses the identical per-shard copies; got {:?}",
            sboms[0].target
        );
    }

    /// The external (syft) archive path is UNTOUCHED by the built-in dedupe:
    /// per-archive scanning produces genuinely-distinct SBOMs, so two archives
    /// yield two SBOMs (one rendered document each).
    #[cfg(unix)]
    #[test]
    fn external_cmd_archive_emits_one_sbom_per_archive() {
        let tools = FakeToolDir::new();
        tools
            .tool("syft")
            .script("for a in \"$@\"; do case \"$a\" in *=*) echo '{}' > \"${a#*=}\";; esac; done")
            .install();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let dist = tmpdir.path().to_path_buf();
        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(dist.clone())
            .add_sbom(SbomConfig {
                id: Some("perarch".into()),
                cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
                artifacts: Some("archive".into()),
                documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
                args: Some(vec![
                    "scan".into(),
                    "$artifact".into(),
                    "--output".into(),
                    "spdx-json=$document".into(),
                ]),
                env: Some(vec![]),
                ..Default::default()
            })
            .build();

        for (name, target) in [
            ("pkg-linux.tar.gz", "x86_64-unknown-linux-gnu"),
            ("pkg-mac.tar.gz", "aarch64-apple-darwin"),
        ] {
            let p = dist.join(name);
            std::fs::write(&p, b"archive").unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                name: name.into(),
                path: p,
                target: Some(target.into()),
                crate_name: "myproj".into(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        SbomStage.run(&mut ctx).expect("external sbom stage");

        let mut names: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "pkg-linux.tar.gz.spdx.json".to_string(),
                "pkg-mac.tar.gz.spdx.json".to_string(),
            ],
            "external (syft) archive path must stay per-archive — one distinct \
             SBOM per scanned archive"
        );
    }

    /// An unknown `artifacts:` value is rejected with the valid-values hint.
    /// The tool is never invoked.
    #[cfg(unix)]
    #[test]
    fn external_cmd_unknown_artifacts_type_bails() {
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("bogus".into()),
                artifacts: Some("nonsense".into()),
                documents: Some(vec!["x.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            },
        );

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("unknown artifacts type must bail");
        let chain = format!("{err:#}");
        assert!(chain.contains("sbom[bogus]"), "got: {chain}");
        assert!(chain.contains("unknown artifacts type"), "got: {chain}");
        assert!(chain.contains("nonsense"), "got: {chain}");
        assert!(!tools.was_called("syft"));
    }

    /// Dry-run never spawns the tool but still returns Ok.
    #[cfg(unix)]
    #[test]
    fn external_cmd_dry_run_does_not_spawn() {
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(tmpdir.path().to_path_buf())
            .dry_run(true)
            .add_sbom(SbomConfig {
                id: Some("dry".into()),
                cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            })
            .build();

        SbomStage.run(&mut ctx).expect("dry-run sbom stage");
        assert!(
            !tools.was_called("syft"),
            "dry-run must not invoke the tool"
        );
    }

    /// `skip: true` short-circuits before any tool spawn.
    #[cfg(unix)]
    #[test]
    fn external_cmd_skip_true_does_not_spawn() {
        use anodizer_core::config::StringOrBool;
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let (mut ctx, _tmp) = external_ctx(
            tools.tool_path("syft"),
            SbomConfig {
                id: Some("skipper".into()),
                artifacts: Some("any".into()),
                documents: Some(vec!["bom.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        );

        SbomStage.run(&mut ctx).expect("skipped sbom stage");
        assert!(!tools.was_called("syft"), "skip:true must not run the tool");
    }

    /// Two SBOM configs sharing the same resolved id is a config error caught
    /// before any subprocess runs.
    #[cfg(unix)]
    #[test]
    fn duplicate_sbom_ids_bail() {
        let tools = FakeToolDir::new();
        tools.tool("syft").install();

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let cmd = tools.tool_path("syft").to_string_lossy().into_owned();
        let mut ctx = TestContextBuilder::new()
            .project_name("myproj")
            .tag("v1.0.0")
            .dist(tmpdir.path().to_path_buf())
            .add_sbom(SbomConfig {
                id: Some("dup".into()),
                cmd: Some(cmd.clone()),
                artifacts: Some("any".into()),
                documents: Some(vec!["a.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            })
            .add_sbom(SbomConfig {
                id: Some("dup".into()),
                cmd: Some(cmd),
                artifacts: Some("any".into()),
                documents: Some(vec!["b.spdx.json".into()]),
                args: Some(vec!["scan".into()]),
                env: Some(vec![]),
                ..Default::default()
            })
            .build();

        let err = SbomStage
            .run(&mut ctx)
            .expect_err("duplicate ids must bail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("multiple sboms with the ID 'dup'"),
            "got: {chain}"
        );
        assert!(!tools.was_called("syft"));
    }

    // -----------------------------------------------------------------------
    // Pure-helper coverage: parse_cargo_lock, find_cargo_lock, SPDX shape,
    // deterministic_uuid_from.
    // -----------------------------------------------------------------------

    /// `parse_cargo_lock` extracts name/version/source for each `[[package]]`
    /// and tolerates a missing `source` (path/workspace members).
    #[test]
    fn parse_cargo_lock_extracts_packages() {
        let lock = r#"
version = 3

[[package]]
name = "anyhow"
version = "1.0.86"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "localcrate"
version = "0.1.0"
"#;
        let pkgs = parse_cargo_lock(lock).expect("parse");
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "anyhow");
        assert_eq!(pkgs[0].version, "1.0.86");
        assert_eq!(
            pkgs[0].source.as_deref(),
            Some("registry+https://github.com/rust-lang/crates.io-index")
        );
        assert_eq!(pkgs[1].name, "localcrate");
        assert!(pkgs[1].source.is_none(), "path members have no source");
    }

    /// `parse_cargo_lock` returns an error on non-TOML input rather than
    /// silently yielding an empty package list.
    #[test]
    fn parse_cargo_lock_rejects_invalid_toml() {
        let err = parse_cargo_lock("this is = = not toml ][").expect_err("must reject");
        assert!(format!("{err:#}").contains("Cargo.lock"));
    }

    /// `find_cargo_lock` walks up from a nested dir to the ancestor holding
    /// `Cargo.lock`.
    #[test]
    fn find_cargo_lock_walks_up_to_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "version = 3\n").unwrap();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_cargo_lock(&nested).expect("walk up");
        assert_eq!(found, tmp.path().join("Cargo.lock"));
    }

    /// `find_cargo_lock` bails (naming the start dir) when no ancestor has a
    /// lockfile.
    #[test]
    fn find_cargo_lock_missing_bails() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("no/lock/here");
        std::fs::create_dir_all(&nested).unwrap();
        let err = find_cargo_lock(&nested).expect_err("no lockfile");
        assert!(format!("{err:#}").contains("Cargo.lock not found"));
    }

    /// `generate_spdx` emits a DESCRIBES relationship for the root package and
    /// a DEPENDS_ON + purl externalRef per dependency, and threads the supplied
    /// namespace uuid into `documentNamespace`.
    #[test]
    fn spdx_shape_and_namespace() {
        let pkgs = vec![CargoPackage {
            name: "serde".into(),
            version: "1.0.200".into(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
        }];
        let doc = generate_spdx(
            "myproj",
            "2.0.0",
            "2024-05-06T12:53:20+00:00",
            "NS-UUID",
            &pkgs,
        )
        .unwrap();

        assert_eq!(doc["spdxVersion"], "SPDX-2.3");
        assert_eq!(doc["name"], "myproj-2.0.0");
        assert_eq!(
            doc["documentNamespace"],
            "https://spdx.org/spdxdocs/myproj-2.0.0-NS-UUID"
        );

        let packages = doc["packages"].as_array().unwrap();
        assert_eq!(packages.len(), 2, "root + 1 dependency");
        assert_eq!(packages[1]["name"], "serde");
        assert_eq!(
            packages[1]["externalRefs"][0]["referenceLocator"],
            "pkg:cargo/serde@1.0.200"
        );
        // registry source -> crates.io download location.
        assert_eq!(
            packages[1]["downloadLocation"],
            "https://crates.io/crates/serde/1.0.200"
        );

        let rels = doc["relationships"].as_array().unwrap();
        assert_eq!(rels[0]["relationshipType"], "DESCRIBES");
        assert_eq!(rels[1]["relationshipType"], "DEPENDS_ON");
        assert_eq!(rels[1]["relatedSpdxElement"], "SPDXRef-Package-0");
    }

    /// A non-registry source (git/path) is passed through verbatim as the
    /// SPDX downloadLocation rather than rewritten to a crates.io URL.
    #[test]
    fn spdx_non_registry_source_passthrough() {
        let pkgs = vec![CargoPackage {
            name: "forked".into(),
            version: "0.1.0".into(),
            source: Some("git+https://example.com/forked.git#abc123".into()),
        }];
        let doc = generate_spdx("p", "0", "t", "ns", &pkgs).unwrap();
        assert_eq!(
            doc["packages"][1]["downloadLocation"],
            "git+https://example.com/forked.git#abc123"
        );
    }

    /// `deterministic_uuid_from` is stable for the same seed, differs across
    /// seeds, and has a UUID-v4-shaped layout (version nibble `4`, RFC4122
    /// variant bits in the 8/9/a/b range).
    #[test]
    fn deterministic_uuid_stable_and_shaped() {
        let a = deterministic_uuid_from("myproj-1.0.0");
        let b = deterministic_uuid_from("myproj-1.0.0");
        let c = deterministic_uuid_from("myproj-1.0.1");
        assert_eq!(a, b, "same seed -> same uuid");
        assert_ne!(a, c, "different seed -> different uuid");

        let groups: Vec<&str> = a.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(groups[0].len(), 8);
        assert_eq!(groups[2].len(), 4);
        assert!(groups[2].starts_with('4'), "version nibble must be 4: {a}");
        let variant = groups[3].chars().next().unwrap();
        assert!(
            matches!(variant, '8' | '9' | 'a' | 'b'),
            "RFC4122 variant nibble, got {variant} in {a}"
        );
    }
}
