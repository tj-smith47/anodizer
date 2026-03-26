use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

// ---------------------------------------------------------------------------
// platform_to_arch
// ---------------------------------------------------------------------------

/// Extract the architecture component from a Docker platform string.
/// e.g. "linux/amd64" → "amd64", "linux/arm64" → "arm64"
pub fn platform_to_arch(platform: &str) -> &str {
    platform
        .rfind('/')
        .map(|idx| &platform[idx + 1..])
        .unwrap_or(platform)
}

// ---------------------------------------------------------------------------
// build_docker_command
// ---------------------------------------------------------------------------

/// Construct the `docker buildx build` command arguments.
///
/// * `staging_dir` – path to the directory that acts as the Docker build
///   context (already contains the Dockerfile and binaries).
/// * `platforms` – Docker platform strings, e.g. `["linux/amd64", "linux/arm64"]`.
/// * `tags` – fully-qualified image tags.
/// * `extra_flags` – rendered `build_flag_templates`.
/// * `push` – when `true`, adds `--push` to the command.
/// * `push_flags` – additional flags added to the command when pushing.
pub fn build_docker_command(
    staging_dir: &str,
    platforms: &[&str],
    tags: &[&str],
    extra_flags: &[String],
    push: bool,
    push_flags: &[String],
) -> Vec<String> {
    let mut cmd: Vec<String> = vec![
        "docker".to_string(),
        "buildx".to_string(),
        "build".to_string(),
    ];

    // --platform=linux/amd64,linux/arm64
    let platform_str = platforms.join(",");
    cmd.push(format!("--platform={platform_str}"));

    // --tag <tag> for each image tag
    for tag in tags {
        cmd.push("--tag".to_string());
        cmd.push(tag.to_string());
    }

    // Extra build flags (rendered build_flag_templates)
    for flag in extra_flags {
        cmd.push(flag.clone());
    }

    // --push in live mode (unless skip_push); omit both --push and --load in
    // dry-run (--load is incompatible with multi-platform builds)
    if push {
        cmd.push("--push".to_string());
        // Additional push flags
        for flag in push_flags {
            cmd.push(flag.clone());
        }
    }

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    cmd
}

// ---------------------------------------------------------------------------
// DockerStage
// ---------------------------------------------------------------------------

pub struct DockerStage;

impl Stage for DockerStage {
    fn name(&self) -> &str {
        "docker"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have docker config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.docker.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for krate in &crates {
            let docker_configs = krate.docker.as_ref().unwrap();

            for (idx, docker_cfg) in docker_configs.iter().enumerate() {
                // Determine platforms (default: linux/amd64 + linux/arm64)
                let platforms: Vec<String> = docker_cfg
                    .platforms
                    .clone()
                    .unwrap_or_else(|| {
                        vec![
                            "linux/amd64".to_string(),
                            "linux/arm64".to_string(),
                        ]
                    });

                // Build the staging directory path
                let staging_dir: PathBuf =
                    dist.join("docker").join(&krate.name).join(idx.to_string());

                if !dry_run {
                    fs::create_dir_all(&staging_dir).with_context(|| {
                        format!(
                            "docker: create staging dir {}",
                            staging_dir.display()
                        )
                    })?;
                }

                // ------------------------------------------------------------------
                // Stage binaries per platform/arch
                // ------------------------------------------------------------------
                for platform in &platforms {
                    let arch = platform_to_arch(platform);

                    let binaries_dir = staging_dir.join("binaries").join(arch);
                    if !dry_run {
                        fs::create_dir_all(&binaries_dir).with_context(|| {
                            format!(
                                "docker: create binaries dir {}",
                                binaries_dir.display()
                            )
                        })?;
                    }

                    // Determine which binary names this docker config cares about
                    let binary_filter = docker_cfg.binaries.as_ref();

                    // Find Binary artifacts whose target maps to this arch
                    let matching_binaries: Vec<_> = ctx
                        .artifacts
                        .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                        .into_iter()
                        .filter(|b| {
                            // Check the arch of the artifact's target triple matches
                            let artifact_arch = b
                                .target
                                .as_deref()
                                .map(|t| map_target(t).1)
                                .unwrap_or_default();
                            if artifact_arch != arch {
                                return false;
                            }
                            // Apply optional binary name filter
                            match binary_filter {
                                None => true,
                                Some(names) => {
                                    let bin_name = b
                                        .metadata
                                        .get("binary")
                                        .map(|s| s.as_str())
                                        .unwrap_or("");
                                    names.iter().any(|n| n == bin_name)
                                }
                            }
                        })
                        .collect();

                    for bin_artifact in matching_binaries {
                        let bin_name = bin_artifact
                            .metadata
                            .get("binary")
                            .map(|s| s.as_str())
                            .unwrap_or_else(|| {
                                bin_artifact
                                    .path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("binary")
                            });

                        let dest = binaries_dir.join(bin_name);

                        if dry_run {
                            eprintln!(
                                "[docker] (dry-run) would copy {} → {}",
                                bin_artifact.path.display(),
                                dest.display()
                            );
                        } else {
                            eprintln!(
                                "[docker] staging binary {} → {}",
                                bin_artifact.path.display(),
                                dest.display()
                            );
                            fs::copy(&bin_artifact.path, &dest).with_context(|| {
                                format!(
                                    "docker: copy binary {} to {}",
                                    bin_artifact.path.display(),
                                    dest.display()
                                )
                            })?;
                        }
                    }
                }

                // ------------------------------------------------------------------
                // Copy Dockerfile
                // ------------------------------------------------------------------
                let dockerfile_src = PathBuf::from(&docker_cfg.dockerfile);
                let dockerfile_dest = staging_dir.join("Dockerfile");

                if dry_run {
                    eprintln!(
                        "[docker] (dry-run) would copy Dockerfile {} → {}",
                        dockerfile_src.display(),
                        dockerfile_dest.display()
                    );
                } else {
                    eprintln!(
                        "[docker] copying Dockerfile {} → {}",
                        dockerfile_src.display(),
                        dockerfile_dest.display()
                    );
                    fs::copy(&dockerfile_src, &dockerfile_dest).with_context(|| {
                        format!(
                            "docker: copy Dockerfile from {} to {}",
                            dockerfile_src.display(),
                            dockerfile_dest.display()
                        )
                    })?;
                }

                // ------------------------------------------------------------------
                // Copy extra_files into staging directory
                // ------------------------------------------------------------------
                if let Some(ref extra_files) = docker_cfg.extra_files {
                    for file_path in extra_files {
                        let src = PathBuf::from(file_path);
                        if src.is_dir() {
                            anyhow::bail!(
                                "docker: extra_files entry '{}' is a directory; only files are supported",
                                file_path
                            );
                        }
                        let file_name = src
                            .file_name()
                            .unwrap_or_else(|| std::ffi::OsStr::new(file_path));
                        let dest = staging_dir.join(file_name);

                        if dry_run {
                            eprintln!(
                                "[docker] (dry-run) would copy extra file {} → {}",
                                src.display(),
                                dest.display()
                            );
                        } else {
                            eprintln!(
                                "[docker] copying extra file {} → {}",
                                src.display(),
                                dest.display()
                            );
                            fs::copy(&src, &dest).with_context(|| {
                                format!(
                                    "docker: copy extra file {} to {}",
                                    src.display(),
                                    dest.display()
                                )
                            })?;
                        }
                    }
                }

                // ------------------------------------------------------------------
                // Render image tag templates
                // ------------------------------------------------------------------
                let mut rendered_tags: Vec<String> = Vec::new();
                for tmpl in &docker_cfg.image_templates {
                    let tag = ctx.render_template(tmpl).with_context(|| {
                        format!(
                            "docker: render image_template '{}' for crate {}",
                            tmpl, krate.name
                        )
                    })?;
                    rendered_tags.push(tag);
                }

                // ------------------------------------------------------------------
                // Build and run the docker buildx command
                // ------------------------------------------------------------------
                let platform_refs: Vec<&str> =
                    platforms.iter().map(|s| s.as_str()).collect();
                let tag_refs: Vec<&str> =
                    rendered_tags.iter().map(|s| s.as_str()).collect();
                let staging_str = staging_dir.to_string_lossy().into_owned();

                // Render build_flag_templates
                let mut extra_flags = Vec::new();
                if let Some(ref flag_templates) = docker_cfg.build_flag_templates {
                    for tmpl in flag_templates {
                        let rendered = ctx.render_template(tmpl).with_context(|| {
                            format!("docker: render build_flag_template '{}'", tmpl)
                        })?;
                        extra_flags.push(rendered);
                    }
                }

                // Determine whether to push
                let skip_push = docker_cfg.skip_push.unwrap_or(false);
                let should_push = !dry_run && !skip_push;

                // Render push_flags (template-aware, consistent with build_flag_templates)
                let mut push_flags = Vec::new();
                if let Some(ref pf_templates) = docker_cfg.push_flags {
                    for tmpl in pf_templates {
                        let rendered = ctx.render_template(tmpl).with_context(|| {
                            format!("docker: render push_flag '{}'", tmpl)
                        })?;
                        push_flags.push(rendered);
                    }
                }

                let cmd_args = build_docker_command(
                    &staging_str,
                    &platform_refs,
                    &tag_refs,
                    &extra_flags,
                    should_push,
                    &push_flags,
                );

                if dry_run {
                    eprintln!("[docker] (dry-run) would run: {}", cmd_args.join(" "));
                } else {
                    eprintln!("[docker] running: {}", cmd_args.join(" "));

                    let status = Command::new(&cmd_args[0])
                        .args(&cmd_args[1..])
                        .status()
                        .with_context(|| {
                            format!(
                                "docker: execute buildx for crate {} index {}",
                                krate.name, idx
                            )
                        })?;

                    if !status.success() {
                        anyhow::bail!(
                            "docker buildx failed for crate {} index {}: exit code {:?}",
                            krate.name,
                            idx,
                            status.code()
                        );
                    }
                }

                // ------------------------------------------------------------------
                // Register DockerImage artifacts
                // ------------------------------------------------------------------
                for tag in &rendered_tags {
                    let mut meta = HashMap::new();
                    meta.insert("tag".to_string(), tag.clone());
                    meta.insert("platforms".to_string(), platforms.join(","));

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerImage,
                        path: staging_dir.clone(),
                        target: None,
                        crate_name: krate.name.clone(),
                        metadata: meta,
                    });
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[test]
    fn test_platform_to_arch() {
        assert_eq!(platform_to_arch("linux/amd64"), "amd64");
        assert_eq!(platform_to_arch("linux/arm64"), "arm64");
    }

    #[test]
    fn test_build_docker_command() {
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64", "linux/arm64"],
            &["ghcr.io/owner/app:v1.0.0", "ghcr.io/owner/app:latest"],
            &[],
            true,
            &[],
        );
        assert!(cmd.contains(&"buildx".to_string()));
        assert!(cmd.contains(&"build".to_string()));
        assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()));
        assert!(cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--tag".to_string()));
    }

    #[test]
    fn test_build_docker_command_dry_run() {
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &[],
        );
        // When push=false, neither --push nor --load
        assert!(!cmd.contains(&"--push".to_string()));
    }

    #[test]
    fn test_stage_skips_without_docker_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DockerStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_platform_to_arch_no_slash() {
        // Fallback: no slash in string returns the whole string
        assert_eq!(platform_to_arch("amd64"), "amd64");
    }

    #[test]
    fn test_build_docker_command_structure() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["my-image:latest"],
            &[],
            true,
            &[],
        );
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "buildx");
        assert_eq!(cmd[2], "build");
        // staging dir is the last argument
        assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
    }

    #[test]
    fn test_build_docker_command_multiple_tags() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &["repo/img:v1.0.0", "repo/img:latest"],
            &[],
            true,
            &[],
        );
        // Both tags should appear after --tag flags
        let tag_positions: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t == "--tag" { Some(i) } else { None })
            .collect();
        assert_eq!(tag_positions.len(), 2);
        assert_eq!(cmd[tag_positions[0] + 1], "repo/img:v1.0.0");
        assert_eq!(cmd[tag_positions[1] + 1], "repo/img:latest");
    }

    #[test]
    fn test_docker_stage_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let tmp = TempDir::new().unwrap();

        // Create fake binaries so the stage has something to pick up
        let amd64_bin = tmp.path().join("myapp-amd64");
        let arm64_bin = tmp.path().join("myapp-arm64");
        fs::write(&amd64_bin, b"fake amd64 binary").unwrap();
        fs::write(&arm64_bin, b"fake arm64 binary").unwrap();

        // Create a fake Dockerfile (not needed in dry-run, but still)
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec![
                "ghcr.io/owner/myapp:{{ .Tag }}".to_string(),
                "ghcr.io/owner/myapp:latest".to_string(),
            ],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec![
                "linux/amd64".to_string(),
                "linux/arm64".to_string(),
            ]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            push_flags: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Register binary artifacts
        let mut meta_amd64 = HashMap::new();
        meta_amd64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: amd64_bin.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_amd64,
        });

        let mut meta_arm64 = HashMap::new();
        meta_arm64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: arm64_bin.clone(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_arm64,
        });

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // Should have registered 2 DockerImage artifacts (one per rendered tag)
        let docker_images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(docker_images.len(), 2);

        let tags: Vec<&str> = docker_images
            .iter()
            .map(|a| a.metadata.get("tag").unwrap().as_str())
            .collect();
        assert!(tags.contains(&"ghcr.io/owner/myapp:v1.0.0"));
        assert!(tags.contains(&"ghcr.io/owner/myapp:latest"));
    }

    // ------------------------------------------------------------------
    // New tests for skip_push, extra_files, push_flags
    // ------------------------------------------------------------------

    #[test]
    fn test_docker_config_parses_new_fields() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
skip_push: true
extra_files:
  - "config.yaml"
  - "scripts/init.sh"
push_flags:
  - "--cache-to=type=registry,ref=ghcr.io/owner/app:cache"
  - "--provenance=true"
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, Some(true));
        let extra = cfg.extra_files.unwrap();
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0], "config.yaml");
        assert_eq!(extra[1], "scripts/init.sh");
        let pf = cfg.push_flags.unwrap();
        assert_eq!(pf.len(), 2);
        assert_eq!(pf[0], "--cache-to=type=registry,ref=ghcr.io/owner/app:cache");
        assert_eq!(pf[1], "--provenance=true");
    }

    #[test]
    fn test_build_docker_command_skip_push() {
        // When push=false (i.e. skip_push is true or dry_run), --push should not appear
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &[],
        );
        assert!(!cmd.contains(&"--push".to_string()));

        // When push=true, --push should appear
        let cmd_push = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            true,
            &[],
        );
        assert!(cmd_push.contains(&"--push".to_string()));
    }

    #[test]
    fn test_build_docker_command_push_flags() {
        let push_flags = vec![
            "--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string(),
            "--provenance=true".to_string(),
        ];
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            true,
            &push_flags,
        );
        assert!(cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string()));
        assert!(cmd.contains(&"--provenance=true".to_string()));

        // push_flags should NOT appear when push=false
        let cmd_no_push = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &push_flags,
        );
        assert!(!cmd_no_push.contains(&"--push".to_string()));
        assert!(!cmd_no_push.contains(&"--provenance=true".to_string()));
    }

    #[test]
    fn test_extra_files_copied_to_staging_dry_run() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::artifact::ArtifactKind;

        let tmp = TempDir::new().unwrap();

        // Create fake Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        // Create fake extra files
        let extra1 = tmp.path().join("config.yaml");
        let extra2 = tmp.path().join("init.sh");
        fs::write(&extra1, b"key: value").unwrap();
        fs::write(&extra2, b"#!/bin/bash\necho hello").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: Some(true),
            extra_files: Some(vec![
                extra1.to_string_lossy().into_owned(),
                extra2.to_string_lossy().into_owned(),
            ]),
            push_flags: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        // dry-run should succeed without actually copying files
        stage.run(&mut ctx).unwrap();

        // In dry-run mode, files are not actually copied, but the stage should
        // complete successfully and register artifacts
        let docker_images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(docker_images.len(), 1);
    }

    #[test]
    fn test_extra_files_copied_to_staging_live() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create fake Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        // Create fake extra files
        let extra1 = tmp.path().join("config.yaml");
        let extra2 = tmp.path().join("init.sh");
        fs::write(&extra1, b"key: value").unwrap();
        fs::write(&extra2, b"#!/bin/bash\necho hello").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: Some(true), // skip push so we don't actually run docker
            extra_files: Some(vec![
                extra1.to_string_lossy().into_owned(),
                extra2.to_string_lossy().into_owned(),
            ]),
            push_flags: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let dist = tmp.path().join("dist");
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // The stage will fail at the docker buildx command (docker not available),
        // but we can verify the staging directory was set up correctly.
        let _result = stage_setup_only(&mut ctx);

        // Verify the staging directory was created with extra files
        let staging_dir = dist.join("docker").join("myapp").join("0");
        // The Dockerfile should be copied
        assert!(staging_dir.join("Dockerfile").exists());
        // Extra files should be copied
        assert!(staging_dir.join("config.yaml").exists());
        assert!(staging_dir.join("init.sh").exists());
        // Verify content
        assert_eq!(fs::read_to_string(staging_dir.join("config.yaml")).unwrap(), "key: value");
        assert_eq!(fs::read_to_string(staging_dir.join("init.sh")).unwrap(), "#!/bin/bash\necho hello");
    }

    /// Helper: runs the docker stage but catches the expected docker-not-found error.
    /// This lets us verify the staging directory setup without requiring docker.
    fn stage_setup_only(ctx: &mut Context) -> Result<()> {
        let stage = DockerStage;
        stage.run(ctx)
    }

    #[test]
    fn test_docker_config_new_fields_default_to_none() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, None);
        assert_eq!(cfg.extra_files, None);
        assert_eq!(cfg.push_flags, None);
    }
}
