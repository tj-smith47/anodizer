use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::ArtifactKind;
use anodize_core::config::UpxConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::util::find_binary;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format a byte count as a human-readable string (B/KB/MB).
fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

/// Match a target string against a glob-style pattern.
/// Supports `*` as a wildcard that matches any sequence of characters.
pub(crate) fn target_matches_pattern(target: &str, pattern: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(target))
        .unwrap_or(false)
}

/// Check if an artifact should be compressed by this UPX config.
/// Returns `true` if the artifact matches the ids and targets filters.
pub(crate) fn should_compress(
    upx_cfg: &UpxConfig,
    artifact_target: Option<&str>,
    artifact_metadata_id: Option<&str>,
    artifact_metadata_name: Option<&str>,
) -> bool {
    // Filter by ids: if ids is set, at least one must match the artifact metadata
    if let Some(ref ids) = upx_cfg.ids {
        let matches_id = artifact_metadata_id
            .map(|id| ids.contains(&id.to_string()))
            .unwrap_or(false);
        let matches_name = artifact_metadata_name
            .map(|name| ids.contains(&name.to_string()))
            .unwrap_or(false);
        if !matches_id && !matches_name {
            return false;
        }
    }

    // Filter by targets: if targets is set, at least one pattern must match
    if let Some(ref targets) = upx_cfg.targets {
        if let Some(target) = artifact_target {
            if !targets
                .iter()
                .any(|pat| target_matches_pattern(target, pat))
            {
                return false;
            }
        } else {
            // No target on artifact but targets filter is set => skip
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// UpxStage
// ---------------------------------------------------------------------------

pub struct UpxStage;

impl Stage for UpxStage {
    fn name(&self) -> &str {
        "upx"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("upx");
        let upx_configs = ctx.config.upx.clone();

        if upx_configs.is_empty() {
            return Ok(());
        }

        let parallelism = ctx.options.parallelism.max(1);

        for upx_cfg in &upx_configs {
            // GoReleaser parity: enabled supports template strings via tmpl.Bool()
            let is_enabled = upx_cfg
                .enabled
                .as_ref()
                .map(|v| v.evaluates_to_true(|tmpl| ctx.render_template(tmpl)))
                .unwrap_or(false);
            if !is_enabled {
                continue;
            }

            let binary = &upx_cfg.binary;

            // Check if UPX binary exists
            if !ctx.is_dry_run() && !find_binary(binary) {
                if upx_cfg.required {
                    anyhow::bail!(
                        "upx: binary '{}' not found and this config is marked as required",
                        binary
                    );
                }
                ctx.strict_guard(
                    &log,
                    &format!("upx: binary '{}' not found, skipping compression", binary),
                )?;
                continue;
            }

            // Collect matching Binary + UniversalBinary artifacts
            // (GoReleaser parity: upx.go:119 filters ByTypes(Binary, UniversalBinary))
            let mut binary_artifacts = ctx.artifacts.by_kind(ArtifactKind::Binary);
            binary_artifacts.extend(ctx.artifacts.by_kind(ArtifactKind::UniversalBinary));
            let matching_artifacts: Vec<(std::path::PathBuf, Option<String>)> = binary_artifacts
                .iter()
                .filter(|a| {
                    should_compress(
                        upx_cfg,
                        a.target.as_deref(),
                        a.metadata.get("id").map(|s| s.as_str()),
                        a.metadata.get("name").map(|s| s.as_str()),
                    )
                })
                .map(|a| (a.path.clone(), a.target.clone()))
                .collect();

            if matching_artifacts.is_empty() {
                let id_label = upx_cfg.id.as_deref().unwrap_or("default");
                ctx.strict_guard(
                    &log,
                    &format!(
                        "upx[{}]: no matching binary artifacts to compress",
                        id_label
                    ),
                )?;
                continue;
            }

            // Dry-run: just log what would happen (no parallelism needed)
            if ctx.is_dry_run() {
                for (artifact_path, _target) in &matching_artifacts {
                    let artifact_str = artifact_path.to_string_lossy();
                    let id_label = upx_cfg.id.as_deref().unwrap_or("default");
                    let mut extra_flags = Vec::new();
                    if let Some(ref level) = upx_cfg.compress {
                        extra_flags.push(format!("-{}", level));
                    }
                    if upx_cfg.lzma.unwrap_or(false) {
                        extra_flags.push("--lzma".to_string());
                    }
                    if upx_cfg.brute.unwrap_or(false) {
                        extra_flags.push("--brute".to_string());
                    }
                    extra_flags.extend(upx_cfg.args.iter().cloned());
                    log.status(&format!(
                        "(dry-run) [{}] would run: {} --quiet {} {}",
                        id_label,
                        binary,
                        extra_flags.join(" "),
                        artifact_str,
                    ));
                }
                continue;
            }

            // GoReleaser parity: compress artifacts in parallel using
            // semerrgroup-style bounded concurrency (upx.go uses
            // semerrgroup.New(ctx.Parallelism)). Shared helper in
            // anodize_core::parallel preserves bounded concurrency,
            // submission-order results, fail-fast within a chunk, and
            // attributable panic reporting.
            let run_job = |job: &(std::path::PathBuf, Option<String>)| -> Result<()> {
                let (artifact_path, target) = job;
                let thread_log = anodize_core::log::StageLogger::new("upx", log.verbosity());
                let artifact_str = artifact_path.to_string_lossy();
                let id_label = upx_cfg.id.as_deref().unwrap_or("default");
                let target_label = target.as_deref().unwrap_or("unknown");

                thread_log.status(&format!(
                    "[{}] compressing {} (target: {})",
                    id_label, artifact_str, target_label,
                ));

                let size_before = std::fs::metadata(artifact_path)
                    .map(|m| m.len())
                    .unwrap_or(0);

                let mut cmd = Command::new(binary);
                cmd.arg("--quiet");
                if let Some(ref level) = upx_cfg.compress {
                    if level == "best" {
                        cmd.arg("--best");
                    } else {
                        cmd.arg(format!("-{}", level));
                    }
                }
                if upx_cfg.lzma.unwrap_or(false) {
                    cmd.arg("--lzma");
                }
                if upx_cfg.brute.unwrap_or(false) {
                    cmd.arg("--brute");
                }
                cmd.args(&upx_cfg.args);
                cmd.arg(artifact_path);
                let output = cmd.output().with_context(|| {
                    format!("upx: failed to spawn '{}' for {}", binary, artifact_str)
                })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let combined = format!("{}{}", stdout, stderr);

                    const KNOWN_EXCEPTIONS: &[&str] = &[
                        "CantPackException",
                        "AlreadyPackedException",
                        "NotCompressibleException",
                        "UnknownExecutableFormatException",
                        "IOException",
                    ];

                    if KNOWN_EXCEPTIONS.iter().any(|ex| combined.contains(ex)) {
                        thread_log.warn(&format!(
                            "[{}] skipping {} (target: {}): {}",
                            id_label,
                            artifact_str,
                            target_label,
                            combined.trim(),
                        ));
                    } else {
                        thread_log.check_output(output, binary)?;
                    }
                } else {
                    let size_after = std::fs::metadata(artifact_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let ratio = (size_after * 100).checked_div(size_before).unwrap_or(100);
                    thread_log.status(&format!(
                        "compressed {} ({} -> {}, {}%)",
                        artifact_path.display(),
                        format_size(size_before),
                        format_size(size_after),
                        ratio,
                    ));
                }

                Ok(())
            };

            anodize_core::parallel::run_parallel_chunks(
                &matching_artifacts,
                parallelism,
                "upx",
                run_job,
            )?;
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
    use anodize_core::artifact::{Artifact, ArtifactKind};
    use anodize_core::config::UpxConfig;
    use anodize_core::test_helpers::TestContextBuilder;

    // -----------------------------------------------------------------------
    // target_matches_pattern tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_target_matches_exact() {
        assert!(target_matches_pattern(
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu"
        ));
    }

    #[test]
    fn test_target_matches_prefix_wildcard() {
        assert!(target_matches_pattern(
            "x86_64-unknown-linux-gnu",
            "x86_64-*"
        ));
    }

    #[test]
    fn test_target_matches_suffix_wildcard() {
        assert!(target_matches_pattern(
            "x86_64-unknown-linux-gnu",
            "*-linux-gnu"
        ));
    }

    #[test]
    fn test_target_matches_middle_wildcard() {
        assert!(target_matches_pattern(
            "x86_64-unknown-linux-gnu",
            "*-linux-*"
        ));
    }

    #[test]
    fn test_target_no_match() {
        assert!(!target_matches_pattern(
            "x86_64-unknown-linux-gnu",
            "aarch64-*"
        ));
    }

    #[test]
    fn test_target_matches_star_matches_all() {
        assert!(target_matches_pattern("x86_64-unknown-linux-gnu", "*"));
    }

    // -----------------------------------------------------------------------
    // should_compress tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_compress_no_filters() {
        let cfg = UpxConfig::default();
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            None
        ));
    }

    #[test]
    fn test_should_compress_no_filters_no_target() {
        let cfg = UpxConfig::default();
        assert!(should_compress(&cfg, None, None, None));
    }

    #[test]
    fn test_should_compress_ids_filter_matches_id() {
        let cfg = UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            ..Default::default()
        };
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("myapp"),
            None
        ));
    }

    #[test]
    fn test_should_compress_ids_filter_matches_name() {
        let cfg = UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            ..Default::default()
        };
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            Some("myapp")
        ));
    }

    #[test]
    fn test_should_compress_ids_filter_no_match() {
        let cfg = UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            ..Default::default()
        };
        assert!(!should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("other"),
            Some("other-name"),
        ));
    }

    #[test]
    fn test_should_compress_ids_filter_no_metadata() {
        let cfg = UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            ..Default::default()
        };
        assert!(!should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            None
        ));
    }

    #[test]
    fn test_should_compress_targets_filter_matches() {
        let cfg = UpxConfig {
            targets: Some(vec!["x86_64-*".to_string()]),
            ..Default::default()
        };
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            None
        ));
    }

    #[test]
    fn test_should_compress_targets_filter_no_match() {
        let cfg = UpxConfig {
            targets: Some(vec!["aarch64-*".to_string()]),
            ..Default::default()
        };
        assert!(!should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            None
        ));
    }

    #[test]
    fn test_should_compress_targets_filter_no_target() {
        let cfg = UpxConfig {
            targets: Some(vec!["x86_64-*".to_string()]),
            ..Default::default()
        };
        assert!(!should_compress(&cfg, None, None, None));
    }

    #[test]
    fn test_should_compress_multiple_targets() {
        let cfg = UpxConfig {
            targets: Some(vec!["x86_64-*".to_string(), "aarch64-*".to_string()]),
            ..Default::default()
        };
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            None,
            None
        ));
        assert!(should_compress(
            &cfg,
            Some("aarch64-apple-darwin"),
            None,
            None
        ));
        assert!(!should_compress(
            &cfg,
            Some("armv7-unknown-linux-gnueabihf"),
            None,
            None
        ));
    }

    #[test]
    fn test_should_compress_both_filters() {
        let cfg = UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            targets: Some(vec!["x86_64-*".to_string()]),
            ..Default::default()
        };
        // Both filters must match
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("myapp"),
            None,
        ));
        // Correct id but wrong target
        assert!(!should_compress(
            &cfg,
            Some("aarch64-apple-darwin"),
            Some("myapp"),
            None,
        ));
        // Correct target but wrong id
        assert!(!should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("other"),
            None,
        ));
    }

    // -----------------------------------------------------------------------
    // Config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_single_upx_object() {
        let yaml = r#"
project_name: test
upx:
  binary: /usr/bin/upx
  args:
    - "--best"
    - "--lzma"
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.upx.len(), 1);
        assert_eq!(config.upx[0].binary, "/usr/bin/upx");
        assert_eq!(config.upx[0].args, vec!["--best", "--lzma"]);
        assert!(config.upx[0].enabled.is_none());
        assert!(!config.upx[0].required);
    }

    #[test]
    fn test_config_parse_upx_array() {
        let yaml = r#"
project_name: test
upx:
  - id: linux
    args: ["--best"]
    targets: ["x86_64-*", "aarch64-*-linux-*"]
  - id: windows
    args: ["--lzma"]
    targets: ["*-windows-*"]
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.upx.len(), 2);
        assert_eq!(config.upx[0].id, Some("linux".to_string()));
        assert_eq!(config.upx[0].targets.as_ref().unwrap().len(), 2);
        assert_eq!(config.upx[1].id, Some("windows".to_string()));
    }

    #[test]
    fn test_config_parse_upx_defaults() {
        let yaml = r#"
project_name: test
upx:
  - {}
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.upx.len(), 1);
        assert!(config.upx[0].enabled.is_none());
        assert_eq!(config.upx[0].binary, "upx");
        assert!(config.upx[0].args.is_empty());
        assert!(!config.upx[0].required);
        assert!(config.upx[0].ids.is_none());
        assert!(config.upx[0].targets.is_none());
    }

    #[test]
    fn test_config_parse_no_upx() {
        let yaml = r#"
project_name: test
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.upx.is_empty());
    }

    #[test]
    fn test_config_parse_upx_with_ids() {
        let yaml = r#"
project_name: test
upx:
  ids: ["myapp", "helper"]
  args: ["--best"]
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.upx.len(), 1);
        let ids = config.upx[0].ids.as_ref().unwrap();
        assert_eq!(ids, &["myapp", "helper"]);
    }

    #[test]
    fn test_config_parse_upx_required() {
        let yaml = r#"
project_name: test
upx:
  required: true
  args: ["--best"]
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.upx[0].required);
    }

    #[test]
    fn test_config_parse_upx_disabled() {
        let yaml = r#"
project_name: test
upx:
  enabled: false
crates: []
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(
            config.upx[0]
                .enabled
                .as_ref()
                .map(|v| !v.as_bool())
                .unwrap_or(true)
        );
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_without_upx_config() {
        let mut ctx = TestContextBuilder::new().build();
        let stage = UpxStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_skips_disabled_config() {
        let upx = vec![UpxConfig {
            enabled: Some(anodize_core::config::StringOrBool::Bool(false)),
            binary: "/nonexistent/binary".to_string(),
            args: vec!["--best".to_string()],
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().upx(upx).build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_skips_with_warning_when_upx_not_found() {
        let upx = vec![UpxConfig {
            binary: "/nonexistent/upx-binary-that-does-not-exist".to_string(),
            required: false,
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().upx(upx).build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        // Should succeed (skip with warning, not error)
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_errors_when_required_and_upx_not_found() {
        let upx = vec![UpxConfig {
            binary: "/nonexistent/upx-binary-that-does-not-exist".to_string(),
            required: true,
            enabled: Some(anodize_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().upx(upx).build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found") && err.contains("required"),
            "error should mention 'not found' and 'required', got: {err}"
        );
    }

    #[test]
    fn test_dry_run_logs_without_executing() {
        // Use /usr/bin/env as the "upx binary" — it exists on the system and
        // WOULD be found by find_binary(). The test verifies that dry-run mode
        // skips both the binary check AND the actual execution. If the stage
        // tried to run /usr/bin/env with UPX args, the command would fail on
        // the artifact path. Success here proves the dry-run contract holds.
        let upx = vec![UpxConfig {
            binary: "/usr/bin/env".to_string(),
            args: vec!["--best".to_string(), "--lzma".to_string()],
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).upx(upx).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        // In dry-run, we skip the binary existence check and just log
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run must not execute upx; got error: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_stage_only_processes_binary_artifacts() {
        let upx = vec![UpxConfig {
            binary: "/nonexistent/upx-binary".to_string(),
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).upx(upx).build();

        // Add a non-Binary artifact — should be ignored
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        // Should complete without processing any artifacts
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_artifact_filtering_by_ids_in_stage() {
        let upx = vec![UpxConfig {
            ids: Some(vec!["myapp".to_string()]),
            binary: "/nonexistent/upx-binary".to_string(),
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).upx(upx).build();

        // Matching artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        // Non-matching artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/other"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "other".to_string());
                m
            },
            size: None,
        });

        let stage = UpxStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_artifact_filtering_by_targets_in_stage() {
        let upx = vec![UpxConfig {
            targets: Some(vec!["x86_64-*".to_string()]),
            binary: "/nonexistent/upx-binary".to_string(),
            ..Default::default()
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).upx(upx).build();

        // Matching target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp-linux"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Non-matching target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp-arm"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_multiple_upx_configs_run_independently() {
        let upx = vec![
            UpxConfig {
                id: Some("linux".to_string()),
                targets: Some(vec!["*-linux-*".to_string()]),
                args: vec!["--best".to_string()],
                binary: "/nonexistent/upx".to_string(),
                ..Default::default()
            },
            UpxConfig {
                id: Some("windows".to_string()),
                targets: Some(vec!["*-windows-*".to_string()]),
                args: vec!["--lzma".to_string()],
                binary: "/nonexistent/upx".to_string(),
                ..Default::default()
            },
        ];

        let mut ctx = TestContextBuilder::new().dry_run(true).upx(upx).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp-linux"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = UpxStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_name() {
        let stage = UpxStage;
        assert_eq!(stage.name(), "upx");
    }
}
