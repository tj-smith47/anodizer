use crate::pipeline;
use anodize_core::artifact;
use anodize_core::config::Config;
use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rich artifact format for split/merge serialization.
/// Mirrors GoReleaser's artifact JSON with OS/arch metadata.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct SplitArtifact {
    /// Artifact filename (basename).
    pub name: String,
    /// Full path to the artifact file.
    pub path: String,
    /// OS component (e.g., "linux", "darwin", "windows").
    pub goos: Option<String>,
    /// Arch component (e.g., "amd64", "arm64").
    pub goarch: Option<String>,
    /// Full target triple (e.g., "x86_64-unknown-linux-gnu").
    pub target: Option<String>,
    /// Artifact kind for internal routing.
    #[serde(rename = "internal_type")]
    pub kind: String,
    /// Human-readable type string.
    #[serde(rename = "type")]
    pub type_s: String,
    /// Crate that produced this artifact.
    pub crate_name: String,
    /// Rich metadata.
    pub extra: HashMap<String, serde_json::Value>,
}

/// Full context serialized during split for merge recovery.
/// Includes config, git info, template vars, and artifacts.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SplitContext {
    /// The partial target that was used for filtering.
    pub partial_target: String,
    /// Template variables (all resolved values at split time).
    pub template_vars: HashMap<String, String>,
    /// Environment variables accessible as {{ Env.VAR }} in templates.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Git info snapshot.
    pub git_tag: Option<String>,
    pub git_commit: Option<String>,
    pub git_branch: Option<String>,
    /// Artifacts produced by this split job.
    pub artifacts: Vec<SplitArtifact>,
}

/// GitHub Actions matrix with runner suggestions.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SplitMatrix {
    /// How the build was split.
    pub split_by: String,
    /// Matrix entries with target and suggested runner.
    pub include: Vec<MatrixEntry>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct MatrixEntry {
    /// OS name (goos mode) or full target triple (target mode).
    pub target: String,
    /// Suggested GitHub Actions runner.
    pub runner: String,
}

/// Convert Artifact to SplitArtifact for serialization.
fn artifact_to_split(a: &artifact::Artifact) -> SplitArtifact {
    SplitArtifact {
        name: a.name().to_string(),
        path: a.path.to_string_lossy().into_owned(),
        goos: a.goos(),
        goarch: a.goarch(),
        target: a.target.clone(),
        kind: a.kind.as_str().to_string(),
        type_s: format!("{:?}", a.kind),
        crate_name: a.crate_name.clone(),
        extra: a
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    }
}

/// Run in --split mode: resolve partial target, build filtered targets,
/// serialize context to dist subdirectory, generate matrix.
pub(super) fn run_split(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    // Resolve partial target from env vars / host detection
    let partial_target = anodize_core::partial::resolve_partial_target(&config.partial)?;
    let subdir = partial_target.dist_subdir();

    log.status(&format!(
        "split mode: building for {} (dist/{})",
        match &partial_target {
            anodize_core::partial::PartialTarget::Exact(t) => t.clone(),
            anodize_core::partial::PartialTarget::OsArch { os, arch } => {
                if let Some(a) = arch {
                    format!("{}/{}", os, a)
                } else {
                    os.clone()
                }
            }
        },
        subdir
    ));

    // Validate that the partial target matches at least one configured build target
    let all_targets = collect_build_targets(config, ctx);
    let matching = partial_target.filter_targets(&all_targets);
    if matching.is_empty() && !all_targets.is_empty() {
        anyhow::bail!(
            "split: no build targets match {}. Available targets: [{}]",
            match &partial_target {
                anodize_core::partial::PartialTarget::Exact(t) => format!("TARGET={}", t),
                anodize_core::partial::PartialTarget::OsArch { os, arch } => {
                    if let Some(a) = arch {
                        format!("ANODIZE_OS={}, ANODIZE_ARCH={}", os, a)
                    } else {
                        format!("ANODIZE_OS={}", os)
                    }
                }
            },
            all_targets.join(", ")
        );
    }

    // Set partial target on context so build stage filters targets
    ctx.options.partial_target = Some(partial_target.clone());

    // Route output to dist subdirectory
    let original_dist = config.dist.clone();
    let split_dist = original_dist.join(&subdir);
    // We modify the config dist in-place so all stages write to the subdirectory
    ctx.config.dist = split_dist.clone();

    std::fs::create_dir_all(&split_dist)
        .with_context(|| format!("create split dist directory: {}", split_dist.display()))?;

    // Run only the build pipeline
    let p = pipeline::build_split_pipeline();
    p.run(ctx, log)?;

    // Copy binary artifacts into the split dist directory so they survive
    // upload/download between split and merge machines.  Update the artifact
    // paths to point at the copies inside dist/.
    //
    // Each artifact goes into a per-target subdirectory (e.g., dist/linux/
    // x86_64-unknown-linux-gnu/cfgd) to prevent filename collisions when
    // multiple architectures produce the same binary name.  Without this,
    // the aarch64 copy would overwrite the x86_64 copy and merge would
    // see only one artifact per OS context.
    for artifact in ctx.artifacts.all_mut() {
        if !artifact.path.exists() {
            continue; // dry-run or already relocated
        }
        if let Some(file_name) = artifact.path.file_name().map(|n| n.to_os_string()) {
            let target_subdir = artifact.target.as_deref().unwrap_or("default");
            let dest_dir = split_dist.join(target_subdir);
            std::fs::create_dir_all(&dest_dir)
                .with_context(|| format!("split: create target dir {}", dest_dir.display()))?;
            let dest = dest_dir.join(&file_name);
            if artifact.path != dest {
                std::fs::copy(&artifact.path, &dest).with_context(|| {
                    format!(
                        "split: copy {} -> {}",
                        artifact.path.display(),
                        dest.display()
                    )
                })?;
                artifact.path = dest;
            }
        }
    }

    // Serialize split context (config + git + template vars + artifacts)
    let split_artifacts: Vec<SplitArtifact> =
        ctx.artifacts.all().iter().map(artifact_to_split).collect();

    let split_ctx = SplitContext {
        partial_target: subdir.clone(),
        template_vars: ctx.template_vars().all().clone(),
        env_vars: ctx.template_vars().all_config_env().clone(),
        git_tag: ctx.template_vars().get("Tag").map(String::from),
        git_commit: ctx.template_vars().get("FullCommit").map(String::from),
        git_branch: ctx.template_vars().get("Branch").map(String::from),
        artifacts: split_artifacts,
    };

    let ctx_path = split_dist.join("context.json");
    let json = serde_json::to_string_pretty(&split_ctx).context("serialize split context")?;
    std::fs::write(&ctx_path, &json)
        .with_context(|| format!("write split context to {}", ctx_path.display()))?;

    log.status(&format!(
        "split: wrote {} artifact(s) + context to {}",
        split_ctx.artifacts.len(),
        ctx_path.display()
    ));

    // Generate matrix.json at the top-level dist directory (not in the subdirectory)
    let all_targets = collect_build_targets(config, ctx);
    if !all_targets.is_empty() {
        let split_by = config
            .partial
            .as_ref()
            .and_then(|p| p.by.as_deref())
            .unwrap_or("goos");

        let matrix = build_matrix(&all_targets, split_by);
        let matrix_json = serde_json::to_string_pretty(&matrix).context("serialize matrix")?;
        let matrix_path = original_dist.join("matrix.json");
        std::fs::create_dir_all(&original_dist)?;
        std::fs::write(&matrix_path, &matrix_json)
            .with_context(|| format!("write matrix to {}", matrix_path.display()))?;
        log.status(&format!(
            "split: wrote matrix to {} ({} entries, split by: {})",
            matrix_path.display(),
            matrix.include.len(),
            split_by
        ));
    }

    Ok(())
}

/// Build a CI matrix from targets, deduplicating by OS when split_by=goos.
fn build_matrix(targets: &[String], split_by: &str) -> SplitMatrix {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for t in targets {
        let entry_target = if split_by == "goos" {
            let (os, _) = anodize_core::target::map_target(t);
            os
        } else {
            t.clone()
        };

        if seen.insert(entry_target.clone()) {
            // For target mode, extract OS component for runner suggestion
            let (os, _) = anodize_core::target::map_target(t);
            let runner = anodize_core::partial::suggest_runner(&os);
            entries.push(MatrixEntry {
                target: entry_target,
                runner: runner.to_string(),
            });
        }
    }

    SplitMatrix {
        split_by: split_by.to_string(),
        include: entries,
    }
}

/// Run in --merge mode: load split contexts, merge artifacts, run post-build stages.
pub fn run_merge(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
    dist_override: Option<&Path>,
) -> Result<()> {
    log.status("running in merge mode (post-build stages)...");

    let dist = dist_override.unwrap_or(&config.dist);

    // Find all context.json files in dist/ subdirectories (new format).
    // Fall back to artifacts.json for backward compat with old split format.
    let context_files = find_split_contexts(dist)?;
    if context_files.is_empty() {
        // Try legacy artifacts.json format
        let artifact_files = find_split_artifacts(dist)?;
        if artifact_files.is_empty() {
            anyhow::bail!(
                "merge: no context.json or artifacts.json files found in {}. \
                 Run `anodize release --split` first.",
                dist.display()
            );
        }
        return run_merge_legacy(ctx, config, log, dry_run, &artifact_files);
    }

    // Load and merge all split contexts
    let mut total_loaded = 0;
    let mut seen_paths = std::collections::HashSet::new();
    let mut first_vars: Option<HashMap<String, String>> = None;

    for ctx_file in &context_files {
        let content = std::fs::read_to_string(ctx_file)
            .with_context(|| format!("read split context: {}", ctx_file.display()))?;
        let split_ctx: SplitContext = serde_json::from_str(&content)
            .with_context(|| format!("parse split context: {}", ctx_file.display()))?;

        // Restore template vars and env vars from first split context
        if first_vars.is_none() {
            for (key, value) in &split_ctx.template_vars {
                ctx.template_vars_mut().set(key, value);
            }
            for (key, value) in &split_ctx.env_vars {
                ctx.template_vars_mut().set_config_env(key, value);
            }
            first_vars = Some(split_ctx.template_vars.clone());
        }

        for sa in &split_ctx.artifacts {
            if !seen_paths.insert(sa.path.clone()) {
                continue;
            }
            let kind = match artifact::ArtifactKind::parse(&sa.kind) {
                Some(k) => k,
                None => {
                    log.warn(&format!(
                        "merge: unknown artifact kind '{}' in {}, skipping",
                        sa.kind,
                        ctx_file.display()
                    ));
                    continue;
                }
            };
            // Convert extra back to flat string metadata
            let metadata: HashMap<String, String> = sa
                .extra
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            ctx.artifacts.add(artifact::Artifact {
                kind,
                name: String::new(),
                path: PathBuf::from(&sa.path),
                target: sa.target.clone(),
                crate_name: sa.crate_name.clone(),
                metadata,
                size: None,
            });
            total_loaded += 1;
        }
    }

    log.status(&format!(
        "merge: loaded {} artifact(s) from {} context(s)",
        total_loaded,
        context_files.len()
    ));

    // Run post-build pipeline
    let p = pipeline::build_merge_pipeline();
    let result = p.run(ctx, log);

    if result.is_ok() {
        super::run_post_pipeline(ctx, config, dry_run, log)?;
    }

    result
}

/// Legacy merge from old-format artifacts.json files.
fn run_merge_legacy(
    ctx: &mut Context,
    config: &Config,
    log: &anodize_core::log::StageLogger,
    dry_run: bool,
    artifact_files: &[PathBuf],
) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct LegacyOutput {
        artifacts: Vec<LegacyArtifact>,
    }
    #[derive(serde::Deserialize)]
    struct LegacyArtifact {
        kind: String,
        path: String,
        target: Option<String>,
        crate_name: String,
        #[serde(default)]
        metadata: HashMap<String, String>,
    }

    let mut total_loaded = 0;
    let mut seen_paths = std::collections::HashSet::new();

    for artifact_file in artifact_files {
        let content = std::fs::read_to_string(artifact_file)
            .with_context(|| format!("read split artifacts: {}", artifact_file.display()))?;
        let output: LegacyOutput = serde_json::from_str(&content)
            .with_context(|| format!("parse split artifacts: {}", artifact_file.display()))?;

        for sa in &output.artifacts {
            if !seen_paths.insert(sa.path.clone()) {
                continue;
            }
            let kind = artifact::ArtifactKind::parse(&sa.kind)
                .ok_or_else(|| anyhow::anyhow!("unknown artifact kind: {}", sa.kind))?;
            ctx.artifacts.add(artifact::Artifact {
                kind,
                name: String::new(),
                path: PathBuf::from(&sa.path),
                target: sa.target.clone(),
                crate_name: sa.crate_name.clone(),
                metadata: sa.metadata.clone(),
                size: None,
            });
            total_loaded += 1;
        }
    }

    log.status(&format!(
        "merge (legacy): loaded {} artifact(s) from {} file(s)",
        total_loaded,
        artifact_files.len()
    ));

    let p = pipeline::build_merge_pipeline();
    let result = p.run(ctx, log);
    if result.is_ok() {
        super::run_post_pipeline(ctx, config, dry_run, log)?;
    }
    result
}

/// Collect all build targets from config for matrix generation.
///
/// Delegates to the shared `commands::helpers::collect_build_targets` so the
/// `anodize targets` CLI and the split pipeline agree on target resolution.
fn collect_build_targets(config: &Config, ctx: &Context) -> Vec<String> {
    crate::commands::helpers::collect_build_targets(config, &ctx.options.selected_crates)
}

/// Find all context.json files in dist/ subdirectories (new split format).
fn find_split_contexts(dist: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    if dist.is_dir()
        && let Ok(entries) = std::fs::read_dir(dist)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let ctx_file = path.join("context.json");
                if ctx_file.exists() {
                    files.push(ctx_file);
                }
            }
        }
    }

    Ok(files)
}

/// Find all artifacts.json files in dist/ (legacy split format).
fn find_split_artifacts(dist: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    let top = dist.join("artifacts.json");
    if top.exists() {
        files.push(top);
    }

    if dist.is_dir()
        && let Ok(entries) = std::fs::read_dir(dist)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let sub_artifacts = path.join("artifacts.json");
                if sub_artifacts.exists() {
                    files.push(sub_artifacts);
                }
            }
        }
    }

    Ok(files)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::CrateConfig;
    use std::collections::HashMap;

    fn make_split_artifact(kind: &str, path: &str, target: Option<&str>) -> SplitArtifact {
        SplitArtifact {
            name: std::path::Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            path: path.to_string(),
            goos: target.map(|t| anodize_core::target::map_target(t).0),
            goarch: target.map(|t| anodize_core::target::map_target(t).1),
            target: target.map(String::from),
            kind: kind.to_string(),
            type_s: kind.to_string(),
            crate_name: "myapp".to_string(),
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_split_artifact_serialization_roundtrip() {
        let artifact =
            make_split_artifact("binary", "/tmp/myapp", Some("x86_64-unknown-linux-gnu"));

        let json = serde_json::to_string(&artifact).unwrap();
        let deserialized: SplitArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, "binary");
        assert_eq!(deserialized.path, "/tmp/myapp");
        assert_eq!(
            deserialized.target.as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(deserialized.goos.as_deref(), Some("linux"));
        assert_eq!(deserialized.goarch.as_deref(), Some("amd64"));
        assert_eq!(deserialized.crate_name, "myapp");
    }

    #[test]
    fn test_split_context_serialization_roundtrip() {
        let ctx = SplitContext {
            partial_target: "linux".to_string(),
            template_vars: HashMap::from([
                ("Tag".to_string(), "v1.0.0".to_string()),
                ("ProjectName".to_string(), "myapp".to_string()),
            ]),
            env_vars: HashMap::from([("GITHUB_TOKEN".to_string(), "ghp_secret".to_string())]),
            git_tag: Some("v1.0.0".to_string()),
            git_commit: Some("abc123".to_string()),
            git_branch: Some("main".to_string()),
            artifacts: vec![
                make_split_artifact("binary", "/tmp/myapp", Some("aarch64-apple-darwin")),
                make_split_artifact("archive", "/tmp/myapp.tar.gz", Some("aarch64-apple-darwin")),
            ],
        };

        let json = serde_json::to_string_pretty(&ctx).unwrap();
        let deserialized: SplitContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.partial_target, "linux");
        assert_eq!(deserialized.template_vars.get("Tag").unwrap(), "v1.0.0");
        assert_eq!(deserialized.git_tag.as_deref(), Some("v1.0.0"));
        assert_eq!(deserialized.artifacts.len(), 2);
        assert_eq!(deserialized.artifacts[0].kind, "binary");
        assert_eq!(deserialized.artifacts[1].kind, "archive");
    }

    #[test]
    fn test_split_context_empty() {
        let ctx = SplitContext {
            partial_target: "linux".to_string(),
            template_vars: HashMap::new(),
            env_vars: HashMap::new(),
            git_tag: None,
            git_commit: None,
            git_branch: None,
            artifacts: vec![],
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: SplitContext = serde_json::from_str(&json).unwrap();
        assert!(deserialized.artifacts.is_empty());
        assert!(deserialized.git_tag.is_none());
    }

    #[test]
    fn test_find_split_artifacts_top_level() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifacts_path = tmp.path().join("artifacts.json");
        std::fs::write(&artifacts_path, "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], artifacts_path);
    }

    #[test]
    fn test_find_split_artifacts_subdirectories() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create subdirectories with artifacts.json
        let linux_dir = tmp.path().join("linux");
        std::fs::create_dir(&linux_dir).unwrap();
        std::fs::write(linux_dir.join("artifacts.json"), "{}").unwrap();

        let darwin_dir = tmp.path().join("darwin");
        std::fs::create_dir(&darwin_dir).unwrap();
        std::fs::write(darwin_dir.join("artifacts.json"), "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_artifacts_both_levels() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Top-level
        std::fs::write(tmp.path().join("artifacts.json"), "{}").unwrap();

        // Subdirectory
        let sub = tmp.path().join("linux");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("artifacts.json"), "{}").unwrap();

        let files = find_split_artifacts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_artifacts_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let files = find_split_artifacts(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_find_split_artifacts_nonexistent_dir() {
        let files = find_split_artifacts(std::path::Path::new("/nonexistent/path")).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_collect_build_targets() {
        use anodize_core::config::BuildConfig;

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                builds: Some(vec![BuildConfig {
                    binary: "myapp".to_string(),
                    targets: Some(vec![
                        "x86_64-unknown-linux-gnu".to_string(),
                        "aarch64-apple-darwin".to_string(),
                    ]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&"x86_64-unknown-linux-gnu".to_string()));
        assert!(targets.contains(&"aarch64-apple-darwin".to_string()));
    }

    #[test]
    fn test_collect_build_targets_deduplicates() {
        use anodize_core::config::BuildConfig;

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![
                CrateConfig {
                    name: "a".to_string(),
                    path: ".".to_string(),
                    builds: Some(vec![BuildConfig {
                        binary: "a".to_string(),
                        targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                CrateConfig {
                    name: "b".to_string(),
                    path: ".".to_string(),
                    builds: Some(vec![BuildConfig {
                        binary: "b".to_string(),
                        targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 1, "should deduplicate targets");
    }

    #[test]
    fn test_collect_build_targets_from_defaults() {
        use anodize_core::config::Defaults;

        let config = Config {
            project_name: "test".to_string(),
            defaults: Some(Defaults {
                targets: Some(vec![
                    "x86_64-unknown-linux-gnu".to_string(),
                    "x86_64-pc-windows-msvc".to_string(),
                ]),
                ..Default::default()
            }),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let ctx = anodize_core::context::Context::new(config.clone(), opts);
        let targets = collect_build_targets(&config, &ctx);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_split_matrix_serialization() {
        let matrix = SplitMatrix {
            split_by: "target".to_string(),
            include: vec![
                MatrixEntry {
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    runner: "ubuntu-latest".to_string(),
                },
                MatrixEntry {
                    target: "aarch64-apple-darwin".to_string(),
                    runner: "macos-latest".to_string(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&matrix).unwrap();
        assert!(json.contains("x86_64-unknown-linux-gnu"));
        assert!(json.contains("ubuntu-latest"));
        assert!(json.contains("macos-latest"));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["include"].is_array());
        assert_eq!(parsed["include"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_matrix_goos_deduplicates() {
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ];
        let matrix = build_matrix(&targets, "goos");
        assert_eq!(matrix.include.len(), 3, "should deduplicate by OS");
        assert_eq!(matrix.include[0].target, "linux");
        assert_eq!(matrix.include[0].runner, "ubuntu-latest");
        assert_eq!(matrix.include[1].target, "darwin");
        assert_eq!(matrix.include[1].runner, "macos-latest");
        assert_eq!(matrix.include[2].target, "windows");
        assert_eq!(matrix.include[2].runner, "windows-latest");
    }

    #[test]
    fn test_build_matrix_target_no_dedup() {
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        let matrix = build_matrix(&targets, "target");
        assert_eq!(
            matrix.include.len(),
            2,
            "target mode should not deduplicate"
        );
    }

    #[test]
    fn test_find_split_contexts() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create subdirectories with context.json
        let linux_dir = tmp.path().join("linux");
        std::fs::create_dir(&linux_dir).unwrap();
        std::fs::write(linux_dir.join("context.json"), "{}").unwrap();

        let darwin_dir = tmp.path().join("darwin");
        std::fs::create_dir(&darwin_dir).unwrap();
        std::fs::write(darwin_dir.join("context.json"), "{}").unwrap();

        let files = find_split_contexts(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_split_contexts_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let files = find_split_contexts(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_split_merge_artifact_kind_roundtrip() {
        use anodize_core::artifact::ArtifactKind;

        // All artifact kinds should round-trip through as_str/from_str
        let kinds = [
            ArtifactKind::Binary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::DockerImage,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Metadata,
            ArtifactKind::Library,
            ArtifactKind::Wasm,
            ArtifactKind::SourceArchive,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
        ];
        for kind in &kinds {
            let s = kind.as_str();
            let parsed = ArtifactKind::parse(s);
            assert!(
                parsed.is_some(),
                "ArtifactKind::parse({:?}) should succeed",
                s
            );
            assert_eq!(*kind, parsed.unwrap());
        }
    }

    #[test]
    fn test_artifact_kind_from_str_unknown() {
        use anodize_core::artifact::ArtifactKind;
        assert!(ArtifactKind::parse("unknown_kind").is_none());
        assert!(ArtifactKind::parse("").is_none());
    }
}
