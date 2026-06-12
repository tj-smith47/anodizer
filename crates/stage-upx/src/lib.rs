use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::UpxConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::util::find_binary;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use anodizer_core::artifact::format_size;

/// Match a target string against a glob-style pattern.
/// Supports `*` as a wildcard that matches any sequence of characters.
pub(crate) fn target_matches_pattern(target: &str, pattern: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(target))
        .unwrap_or(false)
}

/// Validate the `compress` value against UPX's accepted set: empty (use UPX
/// defaults), `best`, or one of `1`..=`9`. Returning `Err` here surfaces the
/// typo before we shell out and UPX rejects it with an unhelpful exit code.
pub fn validate_compress(level: Option<&str>) -> Result<()> {
    let Some(level) = level else { return Ok(()) };
    if level.is_empty() || level == "best" {
        return Ok(());
    }
    if matches!(level, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") {
        return Ok(());
    }
    anyhow::bail!("upx: compress {level:?} is invalid (use \"best\", \"1\"..=\"9\", or omit)");
}

/// Check if an artifact should be compressed by this UPX config.
/// Returns `true` if the artifact matches the ids and targets filters.
///
/// Id matching: only the artifact's
/// `id` metadata is consulted (no fallback to `name`).
pub(crate) fn should_compress(
    upx_cfg: &UpxConfig,
    artifact_target: Option<&str>,
    artifact_id: Option<&str>,
) -> bool {
    if let Some(ref ids) = upx_cfg.ids
        && !artifact_id.is_some_and(|id| ids.iter().any(|i| i == id))
    {
        return false;
    }

    if let Some(ref targets) = upx_cfg.targets {
        if let Some(target) = artifact_target {
            if !targets
                .iter()
                .any(|pat| target_matches_pattern(target, pat))
            {
                return false;
            }
        } else {
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
            // enabled supports template strings via tmpl.Bool()
            let is_enabled = match upx_cfg.enabled.as_ref() {
                Some(v) => v
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "upx: render enabled template")?,
                None => false,
            };
            if !is_enabled {
                continue;
            }

            validate_compress(upx_cfg.compress.as_deref())?;

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
                    &format!("skipping upx compression — binary '{}' not found", binary),
                )?;
                continue;
            }

            // Collect matching Binary + UniversalBinary artifacts
            // ).
            //
            // Pipeline ordering: UPX runs after both `build` (per-arch Binary) and
            // `universal` (UniversalBinary lipo'd from per-arch Binaries), so
            // when `replace: true` is set on the universal config the source
            // per-arch binaries have already been removed by the universal stage
            // and only the universal binary is compressed. With `replace: false`
            // both the per-arch and universal binaries appear here and BOTH get
            // compressed in-place.
            let mut binary_artifacts = ctx.artifacts.by_kind(ArtifactKind::Binary);
            binary_artifacts.extend(ctx.artifacts.by_kind(ArtifactKind::UniversalBinary));
            let matching_artifacts: Vec<(std::path::PathBuf, Option<String>)> = binary_artifacts
                .iter()
                .filter(|a| {
                    should_compress(
                        upx_cfg,
                        a.target.as_deref(),
                        a.metadata.get("id").map(|s| s.as_str()),
                    )
                })
                .map(|a| (a.path.clone(), a.target.clone()))
                .collect();

            if matching_artifacts.is_empty() {
                let id_label = upx_cfg.id.as_deref().unwrap_or("default");
                ctx.strict_guard(
                    &log,
                    &format!(
                        "no matching binary artifacts to compress (upx[{}])",
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

            // compress artifacts in parallel using
            // bounded concurrency (
            // semerrgroup.New(ctx.Parallelism)). Shared helper in
            // anodizer_core::parallel preserves bounded concurrency,
            // submission-order results, fail-fast within a chunk, and
            // attributable panic reporting.
            let run_job = |job: &(std::path::PathBuf, Option<String>)| -> Result<()> {
                let (artifact_path, target) = job;
                let thread_log = anodizer_core::log::StageLogger::new("upx", log.verbosity());
                let artifact_str = artifact_path.to_string_lossy();
                let id_label = upx_cfg.id.as_deref().unwrap_or("default");
                let target_label = target.as_deref().unwrap_or("unknown");

                thread_log.status(&format!(
                    "compressing {} (target: {}) (upx[{}])",
                    artifact_str, target_label, id_label,
                ));

                let size_before = match std::fs::metadata(artifact_path) {
                    Ok(m) => m.len(),
                    Err(e) => {
                        anyhow::bail!(
                            "upx: cannot stat artifact {} before compressing: {e}",
                            artifact_path.display()
                        );
                    }
                };
                if size_before == 0 {
                    anyhow::bail!(
                        "upx: artifact {} has zero bytes — refusing to compress \
                         (would emit a misleading 100% ratio)",
                        artifact_path.display()
                    );
                }

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
                            "skipping {} (target: {}) (upx[{}]): {}",
                            artifact_str,
                            target_label,
                            id_label,
                            combined.trim(),
                        ));
                    } else {
                        thread_log.check_output(output, binary)?;
                    }
                } else {
                    let size_after = match std::fs::metadata(artifact_path) {
                        Ok(m) => m.len(),
                        Err(e) => {
                            anyhow::bail!(
                                "upx: cannot stat artifact {} after compressing: {e}",
                                artifact_path.display()
                            );
                        }
                    };
                    let ratio = (size_after * 100) / size_before;
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

            anodizer_core::parallel::run_parallel_chunks(
                &matching_artifacts,
                parallelism,
                "upx",
                run_job,
            )?;
        }

        Ok(())
    }
}

/// Environment requirements for the upx stage: each enabled `upx:` entry's
/// binary (default `upx`). `enabled` defaults to false, matching `run`;
/// a template that fails to render is treated as enabled so a broken
/// expression surfaces in the stage, not as a silently skipped preflight.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let mut out = Vec::new();
    for cfg in &ctx.config.upx {
        let enabled = match cfg.enabled.as_ref() {
            Some(v) => v
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(true),
            None => false,
        };
        if enabled {
            out.push(anodizer_core::EnvRequirement::Tool {
                name: cfg.binary.clone(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::UpxConfig;
    use anodizer_core::test_helpers::TestContextBuilder;

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
            None
        ));
    }

    #[test]
    fn test_should_compress_no_filters_no_target() {
        let cfg = UpxConfig::default();
        assert!(should_compress(&cfg, None, None));
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
            None
        ));
    }

    #[test]
    fn test_should_compress_targets_filter_no_target() {
        let cfg = UpxConfig {
            targets: Some(vec!["x86_64-*".to_string()]),
            ..Default::default()
        };
        assert!(!should_compress(&cfg, None, None));
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
            None
        ));
        assert!(should_compress(&cfg, Some("aarch64-apple-darwin"), None));
        assert!(!should_compress(
            &cfg,
            Some("armv7-unknown-linux-gnueabihf"),
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
        assert!(should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("myapp"),
        ));
        assert!(!should_compress(
            &cfg,
            Some("aarch64-apple-darwin"),
            Some("myapp"),
        ));
        assert!(!should_compress(
            &cfg,
            Some("x86_64-unknown-linux-gnu"),
            Some("other"),
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
            enabled: Some(anodizer_core::config::StringOrBool::Bool(false)),
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
            enabled: Some(anodizer_core::config::StringOrBool::Bool(true)),
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

    // -----------------------------------------------------------------------
    // validate_compress tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_compress_accepts_valid_levels() {
        validate_compress(None).unwrap();
        validate_compress(Some("")).unwrap();
        validate_compress(Some("best")).unwrap();
        for level in ["1", "2", "3", "4", "5", "6", "7", "8", "9"] {
            validate_compress(Some(level)).unwrap();
        }
    }

    #[test]
    fn test_validate_compress_rejects_invalid_levels() {
        for bad in ["0", "10", "fast", "BEST"] {
            let err = validate_compress(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("is invalid"), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: {err}");
        }
    }

    // -----------------------------------------------------------------------
    // env_requirements tests
    // -----------------------------------------------------------------------

    fn ctx_with_upx(upx: Vec<UpxConfig>) -> anodizer_core::context::Context {
        TestContextBuilder::new().upx(upx).build()
    }

    #[test]
    fn test_env_requirements_lists_enabled_binary() {
        let ctx = ctx_with_upx(vec![UpxConfig {
            enabled: Some(anodizer_core::config::StringOrBool::Bool(true)),
            binary: "custom-upx".to_string(),
            ..Default::default()
        }]);
        let reqs = env_requirements(&ctx);
        assert_eq!(reqs.len(), 1);
        match &reqs[0] {
            anodizer_core::EnvRequirement::Tool { name } => assert_eq!(name, "custom-upx"),
            other => panic!("expected Tool requirement, got {other:?}"),
        }
    }

    #[test]
    fn test_env_requirements_skips_disabled_and_default() {
        let ctx = ctx_with_upx(vec![
            UpxConfig::default(),
            UpxConfig {
                enabled: Some(anodizer_core::config::StringOrBool::Bool(false)),
                ..Default::default()
            },
        ]);
        assert!(env_requirements(&ctx).is_empty());
    }

    #[test]
    fn test_env_requirements_treats_broken_template_as_enabled() {
        // A template that fails to render must surface the binary as a
        // requirement so the error is reported by the stage, not hidden by
        // a silently skipped preflight.
        let ctx = ctx_with_upx(vec![UpxConfig {
            enabled: Some(anodizer_core::config::StringOrBool::String(
                "{{ bogus_unclosed".to_string(),
            )),
            binary: "upx".to_string(),
            ..Default::default()
        }]);
        assert_eq!(env_requirements(&ctx).len(), 1);
    }

    // -----------------------------------------------------------------------
    // Live-run tests (stubbed upx binary via FakeToolDir)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    mod live_run {
        use super::*;
        use anodizer_core::config::StringOrBool;
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;

        struct LiveFixture {
            _tmp: tempfile::TempDir,
            tools: FakeToolDir,
            binary_path: std::path::PathBuf,
        }

        impl LiveFixture {
            /// One on-disk binary artifact plus an installed `upx` stub the
            /// config can address by absolute path (no PATH mutation needed).
            fn new(artifact_contents: &[u8]) -> Self {
                let tmp = tempfile::tempdir().unwrap();
                let binary_path = tmp.path().join("myapp");
                std::fs::write(&binary_path, artifact_contents).unwrap();
                let tools = FakeToolDir::new();
                Self {
                    _tmp: tmp,
                    tools,
                    binary_path,
                }
            }

            fn upx_binary(&self) -> String {
                self.tools.tool_path("upx").to_string_lossy().into_owned()
            }

            fn ctx(
                &self,
                cfg_overrides: impl FnOnce(&mut UpxConfig),
            ) -> anodizer_core::context::Context {
                let mut cfg = UpxConfig {
                    enabled: Some(StringOrBool::Bool(true)),
                    binary: self.upx_binary(),
                    ..Default::default()
                };
                cfg_overrides(&mut cfg);
                let mut ctx = TestContextBuilder::new().upx(vec![cfg]).build();
                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Binary,
                    name: "myapp".to_string(),
                    path: self.binary_path.clone(),
                    target: Some("x86_64-unknown-linux-gnu".to_string()),
                    crate_name: "test".to_string(),
                    metadata: Default::default(),
                    size: None,
                });
                ctx
            }
        }

        #[test]
        fn live_run_passes_all_flags_in_order() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|cfg| {
                cfg.compress = Some("best".to_string());
                cfg.lzma = Some(true);
                cfg.brute = Some(true);
                cfg.args = vec!["--ultra-brute".to_string()];
            });

            UpxStage.run(&mut ctx).expect("live run succeeds");

            let calls = fx.tools.calls("upx");
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0],
                vec![
                    "--quiet",
                    "--best",
                    "--lzma",
                    "--brute",
                    "--ultra-brute",
                    &fx.binary_path.to_string_lossy(),
                ]
            );
        }

        #[test]
        fn live_run_numeric_compress_level_uses_dash_n() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|cfg| cfg.compress = Some("7".to_string()));

            UpxStage.run(&mut ctx).expect("live run succeeds");

            let calls = fx.tools.calls("upx");
            assert_eq!(
                calls[0],
                vec!["--quiet", "-7", &fx.binary_path.to_string_lossy() as &str]
            );
        }

        #[test]
        fn live_run_compresses_universal_binaries_too() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|_| {});
            let universal = fx._tmp.path().join("myapp-universal");
            std::fs::write(&universal, b"fat-binary-bytes").unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::UniversalBinary,
                name: "myapp-universal".to_string(),
                path: universal,
                target: Some("universal-apple-darwin".to_string()),
                crate_name: "test".to_string(),
                metadata: Default::default(),
                size: None,
            });

            UpxStage.run(&mut ctx).expect("live run succeeds");
            assert_eq!(fx.tools.call_count("upx"), 2);
        }

        #[test]
        fn live_run_known_exception_skips_artifact_without_error() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools
                .tool("upx")
                .stderr("upx: myapp: CantPackException: superfluous data\n")
                .exit(2)
                .install();
            let mut ctx = fx.ctx(|_| {});

            UpxStage
                .run(&mut ctx)
                .expect("known UPX exceptions must skip, not fail");
            assert_eq!(fx.tools.call_count("upx"), 1);
            // Artifact untouched by the skip.
            assert_eq!(std::fs::read(&fx.binary_path).unwrap(), b"0123456789");
        }

        #[test]
        fn live_run_unknown_failure_propagates_error() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools
                .tool("upx")
                .stderr("upx: fatal: disk on fire\n")
                .exit(1)
                .install();
            let mut ctx = fx.ctx(|_| {});

            let err = UpxStage.run(&mut ctx).unwrap_err().to_string();
            assert!(
                err.contains("disk on fire") || err.contains("exit"),
                "{err}"
            );
        }

        #[test]
        fn live_run_zero_byte_artifact_bails() {
            let fx = LiveFixture::new(b"");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|_| {});

            let err = format!("{:#}", UpxStage.run(&mut ctx).unwrap_err());
            assert!(err.contains("zero bytes"), "{err}");
            assert!(!fx.tools.was_called("upx"), "must bail before spawning");
        }

        #[test]
        fn live_run_missing_artifact_file_bails_on_stat() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            std::fs::remove_file(&fx.binary_path).unwrap();
            let mut ctx = fx.ctx(|_| {});

            let err = format!("{:#}", UpxStage.run(&mut ctx).unwrap_err());
            assert!(err.contains("cannot stat artifact"), "{err}");
        }

        #[test]
        fn live_run_invalid_compress_level_bails_before_spawn() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|cfg| cfg.compress = Some("turbo".to_string()));

            let err = UpxStage.run(&mut ctx).unwrap_err().to_string();
            assert!(err.contains("is invalid"), "{err}");
            assert!(!fx.tools.was_called("upx"));
        }

        #[test]
        fn live_run_missing_binary_skips_with_warning_when_not_required() {
            let fx = LiveFixture::new(b"0123456789");
            // No stub installed: the configured absolute path does not exist.
            let mut ctx = fx.ctx(|cfg| cfg.required = false);

            UpxStage
                .run(&mut ctx)
                .expect("non-required missing upx is a warn-and-skip");
            assert_eq!(std::fs::read(&fx.binary_path).unwrap(), b"0123456789");
        }

        #[test]
        fn live_run_no_matching_artifacts_warns_and_continues() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|cfg| {
                cfg.targets = Some(vec!["aarch64-*".to_string()]);
            });

            UpxStage
                .run(&mut ctx)
                .expect("no-match is a warn, not an error");
            assert!(!fx.tools.was_called("upx"));
        }

        #[test]
        fn live_run_broken_enabled_template_errors() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut ctx = fx.ctx(|cfg| {
                cfg.enabled = Some(StringOrBool::String("{{ bogus_unclosed".to_string()));
            });

            let err = format!("{:#}", UpxStage.run(&mut ctx).unwrap_err());
            assert!(err.contains("render enabled template"), "{err}");
            assert!(!fx.tools.was_called("upx"));
        }

        #[test]
        fn dry_run_logs_flags_without_spawning() {
            let fx = LiveFixture::new(b"0123456789");
            fx.tools.tool("upx").install();
            let mut cfg = UpxConfig {
                enabled: Some(StringOrBool::Bool(true)),
                binary: fx.upx_binary(),
                compress: Some("9".to_string()),
                lzma: Some(true),
                brute: Some(true),
                args: vec!["--ultra-brute".to_string()],
                ..Default::default()
            };
            cfg.id = Some("dry".to_string());
            let mut ctx = TestContextBuilder::new()
                .dry_run(true)
                .upx(vec![cfg])
                .build();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: "myapp".to_string(),
                path: fx.binary_path.clone(),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "test".to_string(),
                metadata: Default::default(),
                size: None,
            });

            UpxStage.run(&mut ctx).expect("dry run succeeds");
            assert!(!fx.tools.was_called("upx"), "dry-run must not spawn upx");
            assert_eq!(std::fs::read(&fx.binary_path).unwrap(), b"0123456789");
        }
    }
}
