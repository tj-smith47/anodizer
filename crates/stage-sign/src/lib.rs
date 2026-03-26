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
                    // contains a matching "id" entry.
                    if let Some(ref ids) = sign_cfg.ids {
                        if let Some(artifact_id) = a.metadata.get("id") {
                            return ids.contains(artifact_id);
                        }
                        return false;
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
    use anodize_core::config::Config;
    use anodize_core::context::{Context, ContextOptions};

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
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_skips_with_empty_signs() {
        let config = Config {
            signs: vec![],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }
}
