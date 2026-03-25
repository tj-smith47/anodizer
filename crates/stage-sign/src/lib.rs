use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::ArtifactKind;
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
/// - `"checksum"` (default) → only `ArtifactKind::Checksum`
pub fn should_sign_artifact(kind: ArtifactKind, filter: &str) -> bool {
    match filter {
        "none" => false,
        "all" => true,
        _ => kind == ArtifactKind::Checksum,
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
        // GPG / generic signing via `sign` config
        // ----------------------------------------------------------------
        if let Some(sign_cfg) = ctx.config.sign.clone() {
            let filter = sign_cfg
                .artifacts
                .as_deref()
                .unwrap_or("checksum");

            if filter != "none" {
                let cmd = sign_cfg
                    .cmd
                    .as_deref()
                    .unwrap_or("gpg")
                    .to_string();

                let args = sign_cfg
                    .args
                    .clone()
                    .unwrap_or_else(|| {
                        vec![
                            "--output".to_string(),
                            "{{ .Signature }}".to_string(),
                            "--detach-sig".to_string(),
                            "{{ .Artifact }}".to_string(),
                        ]
                    });

                // Collect matching artifacts (avoid holding an immutable borrow
                // while we later add new ones, so clone paths up-front).
                let artifact_paths: Vec<std::path::PathBuf> = ctx
                    .artifacts
                    .all()
                    .iter()
                    .filter(|a| should_sign_artifact(a.kind, filter))
                    .map(|a| a.path.clone())
                    .collect();

                for artifact_path in &artifact_paths {
                    let artifact_str = artifact_path.to_string_lossy();
                    let signature_str = format!("{}.sig", artifact_str);

                    let resolved = resolve_sign_args(
                        &args,
                        artifact_str.as_ref(),
                        &signature_str,
                    );

                    if ctx.is_dry_run() {
                        eprintln!(
                            "[sign] (dry-run) would run: {} {}",
                            cmd,
                            resolved.join(" ")
                        );
                        continue;
                    }

                    eprintln!(
                        "[sign] signing {} -> {}",
                        artifact_str,
                        signature_str
                    );

                    let status = Command::new(&cmd)
                        .args(&resolved)
                        .status()
                        .with_context(|| {
                            format!("sign: failed to spawn '{}' for {}", cmd, artifact_str)
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
                    .unwrap_or_else(|| {
                        vec![
                            "sign".to_string(),
                            "{{ .Artifact }}".to_string(),
                        ]
                    });

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

                    let resolved = resolve_sign_args(
                        &args,
                        image_str.as_ref(),
                        &signature_str,
                    );

                    if ctx.is_dry_run() {
                        eprintln!(
                            "[sign] (dry-run) would run: {} {}",
                            cmd,
                            resolved.join(" ")
                        );
                        continue;
                    }

                    eprintln!("[sign] docker-sign {}", image_str);

                    let status = Command::new(&cmd)
                        .args(&resolved)
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
    fn test_stage_skips_without_sign_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }
}
