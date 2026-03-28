use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::process::Command;

use crate::util::{find_windows_artifact, run_cmd_in};

// ---------------------------------------------------------------------------
// WingetManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating a WinGet YAML manifest.
pub struct WingetManifestParams<'a> {
    pub package_id: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub description: &'a str,
    pub license: &'a str,
    pub publisher: &'a str,
    pub publisher_url: &'a str,
    pub url: &'a str,
    pub hash: &'a str,
}

// ---------------------------------------------------------------------------
// Serde structs for WinGet YAML manifest
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct WingetManifest {
    package_identifier: String,
    package_version: String,
    package_name: String,
    publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher_url: Option<String>,
    license: String,
    short_description: String,
    installers: Vec<WingetInstaller>,
    manifest_type: String,
    manifest_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct WingetInstaller {
    architecture: String,
    installer_url: String,
    #[serde(rename = "InstallerSha256")]
    installer_sha256: String,
    installer_type: String,
}

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Generate a WinGet YAML manifest string.
///
/// Produces a singleton-style manifest with the minimum required fields for
/// winget-pkgs submission. Uses `serde_yaml_ng` for proper YAML serialization
/// with correct escaping of special characters.
pub fn generate_manifest(params: &WingetManifestParams<'_>) -> String {
    let manifest = WingetManifest {
        package_identifier: params.package_id.to_string(),
        package_version: params.version.to_string(),
        package_name: params.name.to_string(),
        publisher: params.publisher.to_string(),
        publisher_url: if params.publisher_url.is_empty() {
            None
        } else {
            Some(params.publisher_url.to_string())
        },
        license: params.license.to_string(),
        short_description: params.description.to_string(),
        installers: vec![WingetInstaller {
            architecture: "x64".to_string(),
            installer_url: params.url.to_string(),
            installer_sha256: params.hash.to_string(),
            installer_type: "zip".to_string(),
        }],
        manifest_type: "singleton".to_string(),
        manifest_version: "1.6.0".to_string(),
    };

    serde_yaml_ng::to_string(&manifest).expect("winget: serialize manifest")
}

// ---------------------------------------------------------------------------
// publish_to_winget
// ---------------------------------------------------------------------------

pub fn publish_to_winget(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("winget: crate '{}' not found in config", crate_name))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no publish config for '{}'", crate_name))?;

    let winget_cfg = publish
        .winget
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no winget config for '{}'", crate_name))?;

    let manifests_repo = winget_cfg
        .manifests_repo
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("winget: no manifests_repo config for '{}'", crate_name))?;

    let package_id = winget_cfg.package_identifier.as_ref().ok_or_else(|| {
        anyhow::anyhow!("winget: no package_identifier config for '{}'", crate_name)
    })?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit WinGet manifest for '{}' (pkg={}) to {}/{}",
            crate_name, package_id, manifests_repo.owner, manifests_repo.name
        ));
        return Ok(());
    }

    // Resolve version.
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    let description = winget_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let license = winget_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());
    let publisher_name = winget_cfg
        .publisher
        .clone()
        .unwrap_or_else(|| manifests_repo.owner.clone());
    let publisher_url = winget_cfg.publisher_url.clone().unwrap_or_default();

    // Find the windows Archive artifact.
    let (url, hash) = if let Some(found) = find_windows_artifact(ctx, crate_name) {
        found
    } else {
        anyhow::bail!(
            "winget: no Windows archive artifact found for crate '{}'",
            crate_name
        );
    };

    let manifest = generate_manifest(&WingetManifestParams {
        package_id,
        name: crate_name,
        version: &version,
        description: &description,
        license: &license,
        publisher: &publisher_name,
        publisher_url: &publisher_url,
        url: &url,
        hash: &hash,
    });

    // Clone the winget-pkgs fork using http.extraheader for auth instead of
    // embedding the token in the URL (avoids leaking secrets in process lists
    // and logs).
    let token = ctx
        .options
        .token
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let repo_url = format!(
        "https://github.com/{}/{}.git",
        manifests_repo.owner, manifests_repo.name
    );

    let tmp_dir = tempfile::tempdir().context("winget: create temp dir")?;
    let repo_path = tmp_dir.path();

    // Build git clone command with optional auth header.
    let auth_header;
    let mut clone_args: Vec<&str> = vec!["clone", "--depth=1"];
    if let Some(ref tok) = token {
        auth_header = format!("http.extraheader=Authorization: bearer {}", tok);
        clone_args.extend_from_slice(&["-c", &auth_header]);
    }
    clone_args.push(&repo_url);
    let repo_path_str = repo_path.to_string_lossy();
    clone_args.push(&repo_path_str);

    // We need to use run_cmd without a working dir (not run_cmd_in) for clone.
    let output = Command::new("git")
        .args(&clone_args)
        .output()
        .context("winget: git clone: spawn")?;
    log.check_output(output, "winget: git clone")?;

    // If we used a token, also configure it for subsequent push operations
    // in this repo clone so that push uses the same auth mechanism.
    if let Some(ref tok) = token {
        run_cmd_in(
            repo_path,
            "git",
            &[
                "config",
                "http.extraheader",
                &format!("Authorization: bearer {}", tok),
            ],
            "winget: git config auth",
        )?;
    }

    // Build the manifest path: manifests/<first_char>/<Publisher>/<PackageName>/<version>/
    let first_char = package_id
        .chars()
        .next()
        .unwrap_or('_')
        .to_ascii_lowercase();
    let manifest_dir = repo_path
        .join("manifests")
        .join(first_char.to_string())
        .join(package_id.replace('.', "/"))
        .join(&version);
    std::fs::create_dir_all(&manifest_dir)
        .with_context(|| format!("winget: create manifest dir {}", manifest_dir.display()))?;

    let manifest_file = manifest_dir.join(format!("{}.yaml", package_id));
    std::fs::write(&manifest_file, &manifest)
        .with_context(|| format!("winget: write manifest {}", manifest_file.display()))?;

    log.status(&format!(
        "wrote WinGet manifest: {}",
        manifest_file.display()
    ));

    let branch_name = format!("{}-{}", package_id, version);
    run_cmd_in(
        repo_path,
        "git",
        &["checkout", "-b", &branch_name],
        "winget: git checkout",
    )?;
    run_cmd_in(repo_path, "git", &["add", "."], "winget: git add")?;
    run_cmd_in(
        repo_path,
        "git",
        &[
            "commit",
            "-m",
            &format!("New version: {} version {}", package_id, version),
        ],
        "winget: git commit",
    )?;
    run_cmd_in(
        repo_path,
        "git",
        &["push", "-u", "origin", &branch_name],
        "winget: git push",
    )?;

    log.status(&format!(
        "WinGet manifest pushed to {}/{} branch '{}'",
        manifests_repo.owner, manifests_repo.name, branch_name
    ));

    // Submit PR via GitHub CLI (gh) if available.
    let pr_result = Command::new("gh")
        .current_dir(repo_path)
        .args([
            "pr",
            "create",
            "--repo",
            "microsoft/winget-pkgs",
            "--title",
            &format!("New version: {} version {}", package_id, version),
            "--body",
            &format!(
                "## Package\n- **Package**: {}\n- **Version**: {}\n\nAutomatically submitted by anodize.",
                package_id, version
            ),
            "--head",
            &format!("{}:{}", manifests_repo.owner, branch_name),
        ])
        .output();

    match pr_result {
        Ok(output) if output.status.success() => {
            log.status(&format!(
                "WinGet PR submitted for {} version {}",
                package_id, version
            ));
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log.warn(&format!(
                "winget: gh pr create exited with {} — you may need to create the PR manually{}",
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stderr)
                }
            ));
        }
        Err(e) => {
            log.warn(&format!(
                "winget: could not run gh to create PR: {} — you may need to create the PR manually",
                e
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // generate_manifest tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_manifest_basic() {
        let manifest = generate_manifest(&WingetManifestParams {
            package_id: "Org.MyTool",
            name: "mytool",
            version: "1.0.0",
            description: "A great tool",
            license: "MIT",
            publisher: "My Org",
            publisher_url: "https://example.com",
            url: "https://example.com/mytool-1.0.0-windows-amd64.zip",
            hash: "deadbeef1234567890abcdef",
        });

        assert!(manifest.contains("PackageIdentifier: Org.MyTool"));
        assert!(manifest.contains("PackageVersion: 1.0.0"));
        assert!(manifest.contains("PackageName: mytool"));
        assert!(manifest.contains("Publisher: My Org"));
        assert!(manifest.contains("PublisherUrl: https://example.com"));
        assert!(manifest.contains("License: MIT"));
        assert!(manifest.contains("ShortDescription: A great tool"));
        assert!(manifest.contains("Installers:"));
        assert!(manifest.contains("Architecture: x64"));
        assert!(
            manifest.contains("InstallerUrl: https://example.com/mytool-1.0.0-windows-amd64.zip")
        );
        assert!(manifest.contains("InstallerSha256: deadbeef1234567890abcdef"));
        assert!(manifest.contains("InstallerType: zip"));
        assert!(manifest.contains("ManifestType: singleton"));
        assert!(manifest.contains("ManifestVersion: 1.6.0"));
    }

    #[test]
    fn test_generate_manifest_no_publisher_url() {
        let manifest = generate_manifest(&WingetManifestParams {
            package_id: "Org.Tool",
            name: "tool",
            version: "2.0.0",
            description: "A tool",
            license: "Apache-2.0",
            publisher: "My Org",
            publisher_url: "",
            url: "https://example.com/tool.zip",
            hash: "hash",
        });

        assert!(!manifest.contains("PublisherUrl:"));
        assert!(manifest.contains("Publisher: My Org"));
    }

    #[test]
    fn test_generate_manifest_complete_structure() {
        let manifest = generate_manifest(&WingetManifestParams {
            package_id: "TjSmith.Anodize",
            name: "anodize",
            version: "3.2.1",
            description: "Release automation for Rust projects",
            license: "Apache-2.0",
            publisher: "TJ Smith",
            publisher_url: "https://github.com/tj-smith47",
            url: "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-windows-amd64.zip",
            hash: "aabbccdd11223344",
        });

        // Verify the manifest is well-formed YAML-like text
        let lines: Vec<&str> = manifest.lines().collect();

        // Should start with PackageIdentifier
        assert!(lines[0].starts_with("PackageIdentifier:"));
        // Should end with ManifestVersion
        assert!(lines.last().unwrap().starts_with("ManifestVersion:"));

        // Every line should be non-empty
        for line in &lines {
            assert!(!line.is_empty(), "manifest should not have empty lines");
        }
    }

    #[test]
    fn test_generate_manifest_installer_section() {
        let manifest = generate_manifest(&WingetManifestParams {
            package_id: "Org.App",
            name: "app",
            version: "1.5.0",
            description: "An app",
            license: "MIT",
            publisher: "Publisher",
            publisher_url: "https://example.com",
            url: "https://example.com/app-1.5.0.zip",
            hash: "sha256hash",
        });

        // The Installers section should have proper YAML list format
        assert!(manifest.contains("Installers:\n- Architecture: x64"));

        // InstallerUrl, InstallerSha256, InstallerType should be indented under the list item
        assert!(manifest.contains("  InstallerUrl:"));
        assert!(manifest.contains("  InstallerSha256:"));
        assert!(manifest.contains("  InstallerType: zip"));
    }

    #[test]
    fn test_generate_manifest_yaml_quoting_special_chars() {
        let manifest = generate_manifest(&WingetManifestParams {
            package_id: "Org.Tool",
            name: "tool: the best",
            version: "1.0.0",
            description: "A tool with #special: characters & more",
            license: "MIT",
            publisher: "Publisher",
            publisher_url: "",
            url: "https://example.com/tool.zip",
            hash: "hash",
        });

        // Values with special YAML characters should be quoted by serde_yaml_ng
        assert!(manifest.contains("PackageName: 'tool: the best'"));
        assert!(manifest.contains("ShortDescription: 'A tool with #special: characters & more'"));
    }

    // -----------------------------------------------------------------------
    // publish_to_winget dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_winget_dry_run() {
        use anodize_core::config::{
            Config, CrateConfig, PublishConfig, WingetConfig, WingetManifestsRepoConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: Some("MyOrg.MyTool".to_string()),
                    description: Some("A great tool".to_string()),
                    publisher: Some("My Org".to_string()),
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
        let log = StageLogger::new("publish", Verbosity::Normal);

        // dry-run should succeed without any network/command calls
        assert!(publish_to_winget(&ctx, "mytool", &log).is_ok());
    }

    #[test]
    fn test_publish_to_winget_missing_config() {
        use anodize_core::config::{Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }];

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        // Should fail because there's no winget config
        assert!(publish_to_winget(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_winget_missing_package_identifier() {
        use anodize_core::config::{
            Config, CrateConfig, PublishConfig, WingetConfig, WingetManifestsRepoConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: None, // Missing
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
        let log = StageLogger::new("publish", Verbosity::Normal);

        // Should fail because package_identifier is missing
        assert!(publish_to_winget(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_winget_missing_manifests_repo() {
        use anodize_core::config::{Config, CrateConfig, PublishConfig, WingetConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: None, // Missing
                    package_identifier: Some("Org.Tool".to_string()),
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
        let log = StageLogger::new("publish", Verbosity::Normal);

        // Should fail because manifests_repo is missing
        assert!(publish_to_winget(&ctx, "mytool", &log).is_err());
    }
}
