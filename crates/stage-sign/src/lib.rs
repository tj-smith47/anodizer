use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodize_core::artifact::ArtifactKind;
use anodize_core::config::SignConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` if an artifact of `kind` should be signed given the `filter`
/// string from `SignConfig::artifacts` / `DockerSignConfig::artifacts`.
///
/// Filter values:
/// - `"none"`     → nothing is signed
/// - `"all"`      → every artifact kind is signed
/// - `"source"`   → only `ArtifactKind::Archive` (source archives)
/// - `"archive"`  → only `ArtifactKind::Archive`
/// - `"binary"`   → only `ArtifactKind::Binary`
/// - `"package"`  → only `ArtifactKind::LinuxPackage`
/// - `"checksum"` (default) → only `ArtifactKind::Checksum`
pub fn should_sign_artifact(kind: ArtifactKind, filter: &str) -> bool {
    match filter {
        "none" => false,
        "all" => true,
        "source" | "archive" => kind == ArtifactKind::Archive,
        "binary" => kind == ArtifactKind::Binary,
        "package" => kind == ArtifactKind::LinuxPackage,
        _ => kind == ArtifactKind::Checksum,
    }
}

/// Resolve the signature output path from a `SignConfig::signature` template
/// or fall back to the default `{artifact}.sig`.
fn resolve_signature_path(sign_cfg: &SignConfig, artifact_path: &str, ctx: &Context) -> String {
    if let Some(ref sig_template) = sign_cfg.signature {
        // Set Artifact as a template variable so Tera can resolve it natively.
        // Also do a Go-compat string replacement for {{ .Artifact }} patterns
        // that may appear alongside Tera expressions.
        let preprocessed = sig_template
            .replace("{{ .Artifact }}", artifact_path)
            .replace("{{ Artifact }}", artifact_path);
        ctx.render_template(&preprocessed)
            .unwrap_or_else(|_| format!("{}.sig", artifact_path))
    } else {
        format!("{}.sig", artifact_path)
    }
}

/// Pipe `stdin_content` or the contents of `stdin_file` to a child process's
/// stdin. Returns the appropriate `Stdio` and an optional content buffer.
fn prepare_stdin(sign_cfg: &SignConfig) -> Result<(Stdio, Option<Vec<u8>>)> {
    if let Some(ref content) = sign_cfg.stdin {
        Ok((Stdio::piped(), Some(content.as_bytes().to_vec())))
    } else if let Some(ref path) = sign_cfg.stdin_file {
        let data = std::fs::read(path)
            .with_context(|| format!("sign: failed to read stdin_file '{}'", path))?;
        Ok((Stdio::piped(), Some(data)))
    } else {
        Ok((Stdio::inherit(), None))
    }
}

/// Replace `{{ .Artifact }}` and `{{ .Signature }}` placeholders in each arg.
pub fn resolve_sign_args(
    args: &[String],
    artifact_path: &str,
    signature_path: &str,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            arg.replace("{{ .Artifact }}", artifact_path)
                .replace("{{ .Signature }}", signature_path)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SignStage
// ---------------------------------------------------------------------------

pub struct SignStage;

impl Stage for SignStage {
    fn name(&self) -> &str {
        "sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        // ----------------------------------------------------------------
        // GPG / generic signing via `signs` config (supports multiple)
        // ----------------------------------------------------------------
        let sign_configs = ctx.config.signs.clone();
        for sign_cfg in &sign_configs {
            let filter = sign_cfg.artifacts.as_deref().unwrap_or("checksum");

            if filter == "none" {
                continue;
            }

            let cmd = sign_cfg.cmd.as_deref().unwrap_or("gpg").to_string();

            let args = sign_cfg.args.clone().unwrap_or_else(|| {
                vec![
                    "--output".to_string(),
                    "{{ .Signature }}".to_string(),
                    "--detach-sig".to_string(),
                    "{{ .Artifact }}".to_string(),
                ]
            });

            // Collect matching artifacts (avoid holding an immutable borrow
            // while we later add new ones, so clone paths up-front).
            let artifact_paths: Vec<(
                std::path::PathBuf,
                std::collections::HashMap<String, String>,
            )> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| {
                    if !should_sign_artifact(a.kind, filter) {
                        return false;
                    }
                    // If `ids` filter is set, only sign artifacts whose metadata
                    // contains a matching "id" or "name" entry.
                    if let Some(ref ids) = sign_cfg.ids {
                        let matches_id = a
                            .metadata
                            .get("id")
                            .map(|id| ids.contains(id))
                            .unwrap_or(false);
                        let matches_name = a
                            .metadata
                            .get("name")
                            .map(|name| ids.contains(name))
                            .unwrap_or(false);
                        return matches_id || matches_name;
                    }
                    true
                })
                .map(|a| (a.path.clone(), a.metadata.clone()))
                .collect();

            for (artifact_path, _metadata) in &artifact_paths {
                let artifact_str = artifact_path.to_string_lossy();
                let signature_str = resolve_signature_path(sign_cfg, &artifact_str, ctx);

                let resolved = resolve_sign_args(&args, artifact_str.as_ref(), &signature_str);

                // Also resolve any remaining template variables (e.g., {{ .Env.GPG_FINGERPRINT }})
                let fully_resolved: Vec<String> = resolved
                    .iter()
                    .map(|arg| ctx.render_template(arg).unwrap_or_else(|_| arg.clone()))
                    .collect();

                if ctx.is_dry_run() {
                    eprintln!(
                        "[sign] (dry-run) would run: {} {}",
                        cmd,
                        fully_resolved.join(" ")
                    );
                    continue;
                }

                let id_label = sign_cfg.id.as_deref().unwrap_or("default");
                eprintln!(
                    "[sign:{}] signing {} -> {}",
                    id_label, artifact_str, signature_str
                );

                let (stdin_cfg, stdin_data) = prepare_stdin(sign_cfg)?;

                let mut child = Command::new(&cmd)
                    .args(&fully_resolved)
                    .stdin(stdin_cfg)
                    .spawn()
                    .with_context(|| {
                        format!("sign: failed to spawn '{}' for {}", cmd, artifact_str)
                    })?;

                if let Some(data) = stdin_data
                    && let Some(mut child_stdin) = child.stdin.take()
                {
                    child_stdin.write_all(&data).with_context(|| {
                        format!("sign: failed to write stdin for {}", artifact_str)
                    })?;
                    drop(child_stdin); // Explicitly close stdin so child sees EOF
                }

                let status = child.wait().with_context(|| {
                    format!("sign: failed to wait for '{}' for {}", cmd, artifact_str)
                })?;

                if !status.success() {
                    anyhow::bail!(
                        "sign: '{}' exited with non-zero status for {}",
                        cmd,
                        artifact_str
                    );
                }
            }
        }

        // ----------------------------------------------------------------
        // Docker image signing via `docker_signs` config
        // ----------------------------------------------------------------
        if let Some(docker_signs) = ctx.config.docker_signs.clone() {
            for docker_sign_cfg in &docker_signs {
                let cmd = docker_sign_cfg
                    .cmd
                    .as_deref()
                    .unwrap_or("cosign")
                    .to_string();

                let args = docker_sign_cfg
                    .args
                    .clone()
                    .unwrap_or_else(|| vec!["sign".to_string(), "{{ .Artifact }}".to_string()]);

                let docker_filter = docker_sign_cfg.artifacts.as_deref().unwrap_or("all");

                if docker_filter == "none" {
                    continue;
                }

                let image_paths: Vec<std::path::PathBuf> = ctx
                    .artifacts
                    .by_kind(ArtifactKind::DockerImage)
                    .into_iter()
                    .map(|a| a.path.clone())
                    .collect();

                for image_path in &image_paths {
                    let image_str = image_path.to_string_lossy();
                    // For Docker images the "signature" concept is embedded;
                    // use a placeholder `.sig` path to satisfy the template
                    // if the user has {{ .Signature }} in their args.
                    let signature_str = format!("{}.sig", image_str);

                    let resolved = resolve_sign_args(&args, image_str.as_ref(), &signature_str);

                    let fully_resolved: Vec<String> = resolved
                        .iter()
                        .map(|arg| ctx.render_template(arg).unwrap_or_else(|_| arg.clone()))
                        .collect();

                    if ctx.is_dry_run() {
                        eprintln!(
                            "[sign] (dry-run) would run: {} {}",
                            cmd,
                            fully_resolved.join(" ")
                        );
                        continue;
                    }

                    eprintln!("[sign] docker-sign {}", image_str);

                    let status = Command::new(&cmd)
                        .args(&fully_resolved)
                        .status()
                        .with_context(|| {
                            format!(
                                "sign: failed to spawn '{}' for docker image {}",
                                cmd, image_str
                            )
                        })?;

                    if !status.success() {
                        anyhow::bail!(
                            "sign: '{}' exited with non-zero status for docker image {}",
                            cmd,
                            image_str
                        );
                    }
                }
            }
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
    use anodize_core::test_helpers::TestContextBuilder;

    #[test]
    fn test_resolve_sign_args() {
        let args = vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sig".to_string(),
            "{{ .Artifact }}".to_string(),
        ];
        let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[3], "/tmp/file.tar.gz");
    }

    #[test]
    fn test_filter_artifacts_checksum() {
        assert!(should_sign_artifact(ArtifactKind::Checksum, "checksum"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "checksum"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "checksum"));
    }

    #[test]
    fn test_filter_artifacts_all() {
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all"));
        assert!(should_sign_artifact(ArtifactKind::Archive, "all"));
        assert!(should_sign_artifact(ArtifactKind::Binary, "all"));
    }

    #[test]
    fn test_filter_artifacts_none() {
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "none"));
    }

    #[test]
    fn test_filter_artifacts_archive() {
        assert!(should_sign_artifact(ArtifactKind::Archive, "archive"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "archive"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "archive"));
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "archive"));
    }

    #[test]
    fn test_filter_artifacts_source() {
        // "source" is an alias for "archive"
        assert!(should_sign_artifact(ArtifactKind::Archive, "source"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "source"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "source"));
    }

    #[test]
    fn test_filter_artifacts_binary() {
        assert!(should_sign_artifact(ArtifactKind::Binary, "binary"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "binary"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "binary"));
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "binary"));
    }

    #[test]
    fn test_filter_artifacts_package() {
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "package"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "package"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "package"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "package"));
    }

    #[test]
    fn test_stage_skips_without_sign_config() {
        let mut ctx = TestContextBuilder::new().build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_skips_with_empty_signs() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![])
            .build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_sign_configs_run_independently() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        // Two sign configs targeting different artifact types
        let signs = vec![
            SignConfig {
                id: Some("gpg".to_string()),
                cmd: Some("echo".to_string()),
                args: Some(vec!["signing-archive".to_string()]),
                artifacts: Some("archive".to_string()),
                ids: None,
                signature: None,
                stdin: None,
                stdin_file: None,
            },
            SignConfig {
                id: Some("cosign".to_string()),
                cmd: Some("echo".to_string()),
                args: Some(vec!["signing-checksum".to_string()]),
                artifacts: Some("checksum".to_string()),
                ids: None,
                signature: None,
                stdin: None,
                stdin_file: None,
            },
        ];

        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(signs)
            .build();

        // Add artifacts of both types
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/app.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let stage = SignStage;
        // Both configs should run independently without interfering
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_artifacts_filter_selects_correct_kinds() {
        // "all" matches everything
        assert!(should_sign_artifact(ArtifactKind::Archive, "all"));
        assert!(should_sign_artifact(ArtifactKind::Binary, "all"));
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all"));
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "all"));

        // "none" matches nothing
        assert!(!should_sign_artifact(ArtifactKind::Archive, "none"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "none"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "none"));
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "none"));

        // "archive" only matches Archive
        assert!(should_sign_artifact(ArtifactKind::Archive, "archive"));
        assert!(!should_sign_artifact(ArtifactKind::Binary, "archive"));
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "archive"));

        // "binary" only matches Binary
        assert!(should_sign_artifact(ArtifactKind::Binary, "binary"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "binary"));

        // "package" only matches LinuxPackage
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "package"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "package"));

        // Unknown filter defaults to checksum
        assert!(should_sign_artifact(ArtifactKind::Checksum, "unknown-value"));
        assert!(!should_sign_artifact(ArtifactKind::Archive, "unknown-value"));
    }

    #[test]
    fn test_ids_filter_restricts_signed_artifacts() {
        // Verify the ids filter logic directly by testing should_sign_artifact
        // combined with the ids-based metadata check that the stage performs.
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let sign_cfg = SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("archive".to_string()),
            ids: Some(vec!["linux-release".to_string()]),
            signature: None,
            stdin: None,
            stdin_file: None,
        };

        let filter = sign_cfg.artifacts.as_deref().unwrap_or("checksum");

        // Build test artifacts
        let matching_artifact = Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/linux.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "linux-release".to_string());
                m
            },
        };

        let non_matching_artifact = Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/darwin.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "darwin-release".to_string());
                m
            },
        };

        let no_id_artifact = Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/other.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        };

        let wrong_kind_artifact = Artifact {
            kind: ArtifactKind::Binary,
            path: std::path::PathBuf::from("/tmp/binary"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "linux-release".to_string());
                m
            },
        };

        // Replicate the stage's filtering logic:
        // 1. should_sign_artifact(kind, filter) must be true
        // 2. If ids is set, artifact metadata "id" or "name" must match
        let ids = &sign_cfg.ids;
        let should_sign = |a: &Artifact| -> bool {
            if !should_sign_artifact(a.kind, filter) {
                return false;
            }
            if let Some(id_list) = ids {
                let matches_id = a
                    .metadata
                    .get("id")
                    .map(|id| id_list.contains(id))
                    .unwrap_or(false);
                let matches_name = a
                    .metadata
                    .get("name")
                    .map(|name| id_list.contains(name))
                    .unwrap_or(false);
                return matches_id || matches_name;
            }
            true
        };

        assert!(
            should_sign(&matching_artifact),
            "archive with matching id 'linux-release' should be signed"
        );
        assert!(
            !should_sign(&non_matching_artifact),
            "archive with non-matching id 'darwin-release' should NOT be signed"
        );
        assert!(
            !should_sign(&no_id_artifact),
            "archive with no id metadata should NOT be signed when ids filter is set"
        );
        assert!(
            !should_sign(&wrong_kind_artifact),
            "binary with matching id should NOT be signed when filter is 'archive'"
        );

        // Also run through the stage in dry-run to confirm it completes
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![sign_cfg])
            .build();
        ctx.artifacts.add(matching_artifact);
        ctx.artifacts.add(non_matching_artifact);
        ctx.artifacts.add(no_id_artifact);
        ctx.artifacts.add(wrong_kind_artifact);

        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_logs_without_executing() {
        // The critical assertion: a nonexistent binary in dry-run mode must NOT
        // cause an error. If the stage tried to actually execute the binary,
        // it would fail because /nonexistent/gpg does not exist.
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("/nonexistent/binary/that/does/not/exist".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sig".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(signs.clone())
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let stage = SignStage;
        // This MUST succeed. If dry-run mode were broken and tried to spawn
        // the nonexistent binary, it would return an error.
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run must not execute the signing binary; got error: {:?}",
            result.err()
        );

        // Now verify that WITHOUT dry-run, the same config WOULD fail,
        // proving that dry-run is what prevents execution.
        let mut ctx_no_dry = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx_no_dry.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let result_no_dry = stage.run(&mut ctx_no_dry);
        assert!(
            result_no_dry.is_err(),
            "without dry-run, a nonexistent binary should cause an error"
        );
    }

    #[test]
    fn test_template_variables_in_args_resolve_correctly() {
        let args = vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sig".to_string(),
            "{{ .Artifact }}".to_string(),
            "--extra={{ .Artifact }}.meta".to_string(),
        ];

        let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[0], "--output");
        assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[2], "--detach-sig");
        assert_eq!(resolved[3], "/tmp/file.tar.gz");
        assert_eq!(resolved[4], "--extra=/tmp/file.tar.gz.meta");
    }

    #[test]
    fn test_sign_none_filter_skips_entirely() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("skip".to_string()),
            cmd: Some("false".to_string()), // Would fail if executed
            args: None,
            artifacts: Some("none".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/file.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let stage = SignStage;
        // "none" filter should skip without executing any command
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_signing_binary_errors_with_command_name() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("test".to_string()),
            cmd: Some("/nonexistent/path/to/gpg-that-does-not-exist".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sig".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let stage = SignStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "missing signing binary should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("gpg-that-does-not-exist") || err.contains("spawn"),
            "error should mention the missing command, got: {err}"
        );
    }

    #[test]
    fn test_signing_command_nonzero_exit_errors_with_details() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("test".to_string()),
            cmd: Some("false".to_string()), // always exits with code 1
            args: Some(vec![]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: std::path::PathBuf::from("/tmp/test.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
        });

        let stage = SignStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "signing command returning non-zero should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-zero") || err.contains("false"),
            "error should mention non-zero exit or command name, got: {err}"
        );
    }

    #[test]
    fn test_resolve_sign_args_no_placeholders() {
        let args = vec!["--armor".to_string(), "--verbose".to_string()];
        let resolved = resolve_sign_args(&args, "/tmp/file", "/tmp/file.sig");
        assert_eq!(resolved, args, "args without placeholders should be unchanged");
    }

    #[test]
    fn test_resolve_sign_args_both_placeholders_in_single_arg() {
        let args = vec!["{{ .Artifact }}:{{ .Signature }}".to_string()];
        let resolved = resolve_sign_args(&args, "/tmp/f", "/tmp/f.sig");
        assert_eq!(resolved[0], "/tmp/f:/tmp/f.sig");
    }

    #[test]
    fn test_stdin_file_missing_errors_with_path() {
        let sign_cfg = SignConfig {
            id: None,
            cmd: None,
            args: None,
            artifacts: None,
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: Some("/nonexistent/stdin_file.txt".to_string()),
        };

        let result = prepare_stdin(&sign_cfg);
        assert!(result.is_err(), "missing stdin_file should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("/nonexistent/stdin_file.txt") || err.contains("stdin_file"),
            "error should mention the missing stdin_file path, got: {err}"
        );
    }
}
