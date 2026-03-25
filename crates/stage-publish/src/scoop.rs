use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use std::process::Command;

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Generate a Scoop JSON manifest string for a Windows binary.
pub fn generate_manifest(
    name: &str,
    version: &str,
    url: &str,
    hash: &str,
    description: &str,
    license: &str,
) -> String {
    let manifest = serde_json::json!({
        "version": version,
        "description": description,
        "homepage": format!("https://github.com/{}", name),
        "license": license,
        "architecture": {
            "64bit": {
                "url": url,
                "hash": hash,
                "bin": name
            }
        },
        "checkver": "github",
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": format!(
                        "https://github.com/{0}/{0}/releases/download/v$version/{0}-$version-windows-amd64.zip",
                        name
                    )
                }
            }
        }
    });

    serde_json::to_string_pretty(&manifest).expect("scoop: serialize manifest")
}

// ---------------------------------------------------------------------------
// publish_to_scoop
// ---------------------------------------------------------------------------

pub fn publish_to_scoop(ctx: &Context, crate_name: &str) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("scoop: crate '{}' not found in config", crate_name))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no publish config for '{}'", crate_name))?;

    let scoop_cfg = publish
        .scoop
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no scoop config for '{}'", crate_name))?;

    let bucket = scoop_cfg
        .bucket
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no bucket config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        eprintln!(
            "[publish] (dry-run) would update Scoop bucket {}/{} for '{}'",
            bucket.owner, bucket.name, crate_name
        );
        return Ok(());
    }

    // Resolve version.
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    let description = scoop_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());

    let license = scoop_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());

    // Find the windows-amd64 Archive artifact.
    let windows_artifact = ctx
        .artifacts
        .by_kind_and_crate(
            anodize_core::artifact::ArtifactKind::Archive,
            crate_name,
        )
        .into_iter()
        .find(|a| {
            a.target
                .as_deref()
                .map(|t| t.contains("windows") || t.contains("pc-windows"))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("windows")
        });

    let (url, hash) = if let Some(art) = windows_artifact {
        let url = art
            .metadata
            .get("url")
            .cloned()
            .unwrap_or_else(|| art.path.to_string_lossy().into_owned());
        let hash = art
            .metadata
            .get("sha256")
            .cloned()
            .unwrap_or_default();
        (url, hash)
    } else {
        eprintln!(
            "[publish] scoop: no windows artifact found for '{}', using placeholder URL",
            crate_name
        );
        (
            format!(
                "https://github.com/{0}/{0}/releases/download/v{1}/{0}-{1}-windows-amd64.zip",
                crate_name, version
            ),
            String::new(),
        )
    };

    let manifest = generate_manifest(crate_name, &version, &url, &hash, &description, &license);

    // Clone bucket repo, write manifest, commit, push.
    let token = ctx
        .options
        .token
        .clone()
        .or_else(|| std::env::var("HOMEBREW_TAP_TOKEN").ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let clone_url = if let Some(ref tok) = token {
        format!(
            "https://{}@github.com/{}/{}.git",
            tok, bucket.owner, bucket.name
        )
    } else {
        format!(
            "https://github.com/{}/{}.git",
            bucket.owner, bucket.name
        )
    };

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    run_cmd(
        "git",
        &["clone", "--depth=1", &clone_url, &repo_path.to_string_lossy()],
        "scoop: git clone",
    )?;

    let manifest_path = repo_path.join(format!("{}.json", crate_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    eprintln!("[publish] wrote Scoop manifest: {}", manifest_path.display());

    run_cmd_in(
        repo_path,
        "git",
        &["add", &manifest_path.to_string_lossy()],
        "scoop: git add",
    )?;
    run_cmd_in(
        repo_path,
        "git",
        &[
            "commit",
            "-m",
            &format!("chore: update {} manifest to {}", crate_name, version),
        ],
        "scoop: git commit",
    )?;
    run_cmd_in(repo_path, "git", &["push"], "scoop: git push")?;

    eprintln!(
        "[publish] Scoop bucket {}/{} updated for '{}'",
        bucket.owner, bucket.name, crate_name
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn run_cmd(program: &str, args: &[&str], context_msg: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
    Ok(())
}

fn run_cmd_in(
    dir: &std::path::Path,
    program: &str,
    args: &[&str],
    context_msg: &str,
) -> Result<()> {
    let status = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_manifest() {
        let manifest = generate_manifest(
            "cfgd",
            "1.0.0",
            "https://example.com/cfgd-1.0.0-windows-amd64.zip",
            "sha256xyz",
            "Declarative config management",
            "MIT",
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["architecture"]["64bit"]["hash"], "sha256xyz");
        assert_eq!(json["license"], "MIT");
    }

    #[test]
    fn test_generate_manifest_description() {
        let manifest = generate_manifest(
            "my-tool",
            "2.1.0",
            "https://example.com/my-tool-2.1.0-windows-amd64.zip",
            "deadbeef",
            "A helpful tool",
            "Apache-2.0",
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["description"], "A helpful tool");
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["license"], "Apache-2.0");
        assert_eq!(json["architecture"]["64bit"]["url"], "https://example.com/my-tool-2.1.0-windows-amd64.zip");
    }

    #[test]
    fn test_publish_to_scoop_dry_run() {
        use anodize_core::config::{
            BucketConfig, Config, CrateConfig, PublishConfig, ScoopConfig,
        };
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "cfgd".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "myorg".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: Some("Declarative config management".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // dry-run should succeed without any network/git calls
        assert!(publish_to_scoop(&ctx, "cfgd").is_ok());
    }
}
