use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::process::Command;

use crate::util::{OsArtifact, find_all_platform_artifacts, run_cmd_in};

// ---------------------------------------------------------------------------
// KrewManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating a Krew plugin manifest YAML.
pub struct KrewManifestParams<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub homepage: &'a str,
    pub short_description: &'a str,
    pub description: &'a str,
    pub caveats: &'a str,
    /// `(os, arch, url, sha256, binary_name)` tuples for each platform.
    pub platforms: &'a [KrewPlatform],
}

/// A single platform entry in the Krew manifest.
pub struct KrewPlatform {
    pub os: String,
    pub arch: String,
    pub url: String,
    pub sha256: String,
    pub bin: String,
}

// ---------------------------------------------------------------------------
// Serde structs for Krew YAML manifest
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewManifestYaml {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: KrewMetadata,
    spec: KrewSpec,
}

#[derive(Serialize)]
struct KrewMetadata {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSpec {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    short_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caveats: Option<String>,
    platforms: Vec<KrewPlatformYaml>,
}

#[derive(Serialize)]
struct KrewPlatformYaml {
    selector: KrewSelector,
    uri: String,
    sha256: String,
    bin: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSelector {
    match_labels: KrewMatchLabels,
}

#[derive(Serialize)]
struct KrewMatchLabels {
    os: String,
    arch: String,
}

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Generate a Krew plugin manifest YAML string.
///
/// Uses `serde_yaml_ng` for proper YAML serialization with correct escaping
/// of special characters. The `description` and `caveats` fields use YAML
/// block scalar style (literal `|`) when present, achieved via post-processing.
pub fn generate_manifest(params: &KrewManifestParams<'_>) -> String {
    let platforms: Vec<KrewPlatformYaml> = params
        .platforms
        .iter()
        .map(|p| KrewPlatformYaml {
            selector: KrewSelector {
                match_labels: KrewMatchLabels {
                    os: p.os.clone(),
                    arch: krew_arch(&p.arch).to_string(),
                },
            },
            uri: p.url.clone(),
            sha256: p.sha256.clone(),
            bin: p.bin.clone(),
        })
        .collect();

    let manifest = KrewManifestYaml {
        api_version: "krew.googlecontainertools.github.com/v1alpha2".to_string(),
        kind: "Plugin".to_string(),
        metadata: KrewMetadata {
            name: params.name.to_string(),
        },
        spec: KrewSpec {
            version: format!("v{}", params.version),
            homepage: if params.homepage.is_empty() {
                None
            } else {
                Some(params.homepage.to_string())
            },
            short_description: params.short_description.to_string(),
            description: if params.description.is_empty() {
                None
            } else {
                Some(params.description.to_string())
            },
            caveats: if params.caveats.is_empty() {
                None
            } else {
                Some(params.caveats.to_string())
            },
            platforms,
        },
    };

    serde_yaml_ng::to_string(&manifest).expect("krew: serialize manifest")
}

/// Map the internal arch names to Krew's expected labels.
///
/// This is a publisher-specific mapping layer on top of the generic
/// `infer_arch` in `util.rs`. The util layer produces canonical short
/// forms (`"amd64"`, `"arm64"`), and this function translates them
/// to whatever Krew expects. Today the mapping is a no-op for the
/// common cases, but keeping a separate layer allows adapting to
/// future Krew label changes without touching the shared inference.
fn krew_arch(arch: &str) -> &str {
    match arch {
        "amd64" | "x86_64" => "amd64",
        "arm64" | "aarch64" => "arm64",
        other => other,
    }
}

/// Map the internal OS names to Krew's expected labels.
///
/// See `krew_arch` for the rationale behind keeping a separate mapping
/// layer on top of `infer_os` in `util.rs`.
fn krew_os(os: &str) -> &str {
    match os {
        "darwin" | "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => other,
    }
}

/// Convert `OsArtifact`s into `KrewPlatform`s.
fn artifacts_to_platforms(artifacts: &[OsArtifact], binary_name: &str) -> Vec<KrewPlatform> {
    artifacts
        .iter()
        .map(|a| KrewPlatform {
            os: krew_os(&a.os).to_string(),
            arch: krew_arch(&a.arch).to_string(),
            url: a.url.clone(),
            sha256: a.sha256.clone(),
            bin: binary_name.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// publish_to_krew
// ---------------------------------------------------------------------------

pub fn publish_to_krew(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("krew: crate '{}' not found in config", crate_name))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no publish config for '{}'", crate_name))?;

    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    let manifests_repo = krew_cfg
        .manifests_repo
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no manifests_repo config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit Krew plugin manifest for '{}' to {}/{}",
            crate_name, manifests_repo.owner, manifests_repo.name
        ));
        return Ok(());
    }

    // Resolve version.
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    let description = krew_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let short_description = krew_cfg
        .short_description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let homepage = krew_cfg
        .homepage
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{}/{}", manifests_repo.owner, crate_name));
    let caveats = krew_cfg.caveats.clone().unwrap_or_default();

    // Find artifacts across all platforms.
    let all_artifacts = find_all_platform_artifacts(ctx, crate_name);

    let platforms = if all_artifacts.is_empty() {
        log.warn(&format!(
            "krew: no artifacts found for '{}', using placeholder URLs",
            crate_name
        ));
        vec![KrewPlatform {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            url: format!(
                "https://github.com/{0}/{1}/releases/download/v{2}/{1}-{2}-linux-amd64.tar.gz",
                manifests_repo.owner, crate_name, version
            ),
            sha256: String::new(),
            bin: crate_name.to_string(),
        }]
    } else {
        artifacts_to_platforms(&all_artifacts, crate_name)
    };

    let manifest = generate_manifest(&KrewManifestParams {
        name: crate_name,
        version: &version,
        homepage: &homepage,
        short_description: &short_description,
        description: &description,
        caveats: &caveats,
        platforms: &platforms,
    });

    // Clone the krew-index fork, write the plugin manifest, commit, push.
    let token = ctx
        .options
        .token
        .clone()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());

    let repo_url = format!(
        "https://github.com/{}/{}.git",
        manifests_repo.owner, manifests_repo.name
    );

    let tmp_dir = tempfile::tempdir().context("krew: create temp dir")?;
    let repo_path = tmp_dir.path();

    let auth_header;
    let mut clone_args: Vec<&str> = vec!["clone", "--depth=1"];
    if let Some(ref tok) = token {
        auth_header = format!("http.extraheader=Authorization: bearer {}", tok);
        clone_args.extend_from_slice(&["-c", &auth_header]);
    }
    clone_args.push(&repo_url);
    let repo_path_str = repo_path.to_string_lossy();
    clone_args.push(&repo_path_str);

    let output = Command::new("git")
        .args(&clone_args)
        .output()
        .context("krew: git clone: spawn")?;
    log.check_output(output, "krew: git clone")?;

    if let Some(ref tok) = token {
        run_cmd_in(
            repo_path,
            "git",
            &[
                "config",
                "http.extraheader",
                &format!("Authorization: bearer {}", tok),
            ],
            "krew: git config auth",
        )?;
    }

    // Write plugin manifest under plugins/<name>.yaml.
    let plugins_dir = repo_path.join("plugins");
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("krew: create plugins dir {}", plugins_dir.display()))?;

    let manifest_file = plugins_dir.join(format!("{}.yaml", crate_name));
    std::fs::write(&manifest_file, &manifest)
        .with_context(|| format!("krew: write manifest {}", manifest_file.display()))?;

    log.status(&format!(
        "wrote Krew plugin manifest: {}",
        manifest_file.display()
    ));

    let branch_name = format!("{}-v{}", crate_name, version);
    run_cmd_in(
        repo_path,
        "git",
        &["checkout", "-b", &branch_name],
        "krew: git checkout",
    )?;
    run_cmd_in(repo_path, "git", &["add", "."], "krew: git add")?;
    run_cmd_in(
        repo_path,
        "git",
        &[
            "commit",
            "-m",
            &format!("Add/update {} plugin to v{}", crate_name, version),
        ],
        "krew: git commit",
    )?;
    run_cmd_in(
        repo_path,
        "git",
        &["push", "-u", "origin", &branch_name],
        "krew: git push",
    )?;

    log.status(&format!(
        "Krew manifest pushed to {}/{} branch '{}'",
        manifests_repo.owner, manifests_repo.name, branch_name
    ));

    // Determine the upstream repo to submit the PR against.
    // Use the configured upstream_repo if available, otherwise fall back to
    // the manifests_repo itself (which works when it is the canonical repo
    // rather than a fork).
    let upstream = krew_cfg.upstream_repo.as_ref().unwrap_or(manifests_repo);
    let upstream_slug = format!("{}/{}", upstream.owner, upstream.name);

    // Submit PR via GitHub CLI (gh) if available.
    let pr_result = Command::new("gh")
        .current_dir(repo_path)
        .args([
            "pr",
            "create",
            "--repo",
            &upstream_slug,
            "--title",
            &format!("Add/update {} plugin to v{}", crate_name, version),
            "--body",
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\nAutomatically submitted by anodize.",
                crate_name, version
            ),
            "--head",
            &format!("{}:{}", manifests_repo.owner, branch_name),
        ])
        .output();

    match pr_result {
        Ok(output) if output.status.success() => {
            log.status(&format!(
                "Krew PR submitted for {} v{}",
                crate_name, version
            ));
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log.warn(&format!(
                "krew: gh pr create exited with {} -- you may need to create the PR manually{}",
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
                "krew: could not run gh to create PR: {} -- you may need to create the PR manually",
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
        let manifest = generate_manifest(&KrewManifestParams {
            name: "kubectl-mytool",
            version: "1.0.0",
            homepage: "https://github.com/org/mytool",
            short_description: "A kubectl plugin",
            description: "A great kubectl plugin for managing things.",
            caveats: "",
            platforms: &[
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
                    sha256: "deadbeef".to_string(),
                    bin: "kubectl-mytool".to_string(),
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/mytool-darwin-amd64.tar.gz".to_string(),
                    sha256: "cafebabe".to_string(),
                    bin: "kubectl-mytool".to_string(),
                },
            ],
        });

        assert!(manifest.contains("apiVersion: krew.googlecontainertools.github.com/v1alpha2"));
        assert!(manifest.contains("kind: Plugin"));
        assert!(manifest.contains("  name: kubectl-mytool"));
        assert!(manifest.contains("version: v1.0.0"));
        assert!(manifest.contains("homepage: https://github.com/org/mytool"));
        assert!(manifest.contains("shortDescription: A kubectl plugin"));
        assert!(manifest.contains("A great kubectl plugin for managing things."));
        assert!(!manifest.contains("caveats:"));
        assert!(manifest.contains("platforms:"));
        assert!(manifest.contains("os: linux"));
        assert!(manifest.contains("arch: amd64"));
        assert!(manifest.contains("uri: https://example.com/mytool-linux-amd64.tar.gz"));
        assert!(manifest.contains("sha256: deadbeef"));
        assert!(manifest.contains("bin: kubectl-mytool"));
        assert!(manifest.contains("os: darwin"));
        assert!(manifest.contains("uri: https://example.com/mytool-darwin-amd64.tar.gz"));
        assert!(manifest.contains("sha256: cafebabe"));
    }

    #[test]
    fn test_generate_manifest_with_caveats() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "my-plugin",
            version: "2.0.0",
            homepage: "https://example.com",
            short_description: "Plugin",
            description: "A plugin",
            caveats: "Run 'kubectl my-plugin init' after installation.",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/plugin.tar.gz".to_string(),
                sha256: "hash".to_string(),
                bin: "kubectl-my-plugin".to_string(),
            }],
        });

        assert!(manifest.contains("caveats:"));
        assert!(manifest.contains("Run 'kubectl my-plugin init' after installation."));
    }

    #[test]
    fn test_generate_manifest_no_homepage() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "tool",
            version: "1.0.0",
            homepage: "",
            short_description: "A tool",
            description: "desc",
            caveats: "",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/tool.tar.gz".to_string(),
                sha256: "hash".to_string(),
                bin: "kubectl-tool".to_string(),
            }],
        });

        assert!(!manifest.contains("homepage:"));
    }

    #[test]
    fn test_generate_manifest_multi_platform() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "multi",
            version: "1.0.0",
            homepage: "https://example.com",
            short_description: "Multi-platform plugin",
            description: "A plugin for all platforms.",
            caveats: "",
            platforms: &[
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-linux-amd64.tar.gz".to_string(),
                    sha256: "hash_linux_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                },
                KrewPlatform {
                    os: "linux".to_string(),
                    arch: "arm64".to_string(),
                    url: "https://example.com/multi-linux-arm64.tar.gz".to_string(),
                    sha256: "hash_linux_arm64".to_string(),
                    bin: "kubectl-multi".to_string(),
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-darwin-amd64.tar.gz".to_string(),
                    sha256: "hash_darwin_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                },
                KrewPlatform {
                    os: "darwin".to_string(),
                    arch: "arm64".to_string(),
                    url: "https://example.com/multi-darwin-arm64.tar.gz".to_string(),
                    sha256: "hash_darwin_arm64".to_string(),
                    bin: "kubectl-multi".to_string(),
                },
                KrewPlatform {
                    os: "windows".to_string(),
                    arch: "amd64".to_string(),
                    url: "https://example.com/multi-windows-amd64.zip".to_string(),
                    sha256: "hash_windows_amd64".to_string(),
                    bin: "kubectl-multi".to_string(),
                },
            ],
        });

        // Count platform entries (each starts with "- selector:")
        let platform_count = manifest.matches("- selector:").count();
        assert_eq!(platform_count, 5);

        // Verify all platforms present
        assert!(manifest.contains("hash_linux_amd64"));
        assert!(manifest.contains("hash_linux_arm64"));
        assert!(manifest.contains("hash_darwin_amd64"));
        assert!(manifest.contains("hash_darwin_arm64"));
        assert!(manifest.contains("hash_windows_amd64"));
    }

    #[test]
    fn test_generate_manifest_complete_structure() {
        let manifest = generate_manifest(&KrewManifestParams {
            name: "kubectl-anodize",
            version: "3.2.1",
            homepage: "https://github.com/tj-smith47/anodize",
            short_description: "Release automation as a kubectl plugin",
            description: "A comprehensive release automation tool\nfor Kubernetes-based projects.",
            caveats: "Ensure kubectl is configured before use.",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-linux-amd64.tar.gz".to_string(),
                sha256: "aabbccdd".to_string(),
                bin: "kubectl-anodize".to_string(),
            }],
        });

        // Starts with apiVersion
        assert!(manifest.starts_with("apiVersion:"));

        // Verify structure order
        let lines: Vec<&str> = manifest.lines().collect();
        assert_eq!(
            lines[0],
            "apiVersion: krew.googlecontainertools.github.com/v1alpha2"
        );
        assert_eq!(lines[1], "kind: Plugin");
        assert_eq!(lines[2], "metadata:");
        assert_eq!(lines[3], "  name: kubectl-anodize");
        assert_eq!(lines[4], "spec:");
        assert!(lines[5].contains("version: v3.2.1"));

        // Multi-line description
        assert!(manifest.contains("A comprehensive release automation tool"));
        assert!(manifest.contains("for Kubernetes-based projects."));

        // Caveats
        assert!(manifest.contains("Ensure kubectl is configured before use."));
    }

    // -----------------------------------------------------------------------
    // krew_arch / krew_os helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_krew_arch_mapping() {
        assert_eq!(krew_arch("amd64"), "amd64");
        assert_eq!(krew_arch("x86_64"), "amd64");
        assert_eq!(krew_arch("arm64"), "arm64");
        assert_eq!(krew_arch("aarch64"), "arm64");
        assert_eq!(krew_arch("unknown"), "unknown");
    }

    #[test]
    fn test_krew_os_mapping() {
        assert_eq!(krew_os("darwin"), "darwin");
        assert_eq!(krew_os("macos"), "darwin");
        assert_eq!(krew_os("linux"), "linux");
        assert_eq!(krew_os("windows"), "windows");
        assert_eq!(krew_os("freebsd"), "freebsd");
    }

    // -----------------------------------------------------------------------
    // publish_to_krew dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_krew_dry_run() {
        use anodize_core::config::{
            Config, CrateConfig, KrewConfig, KrewManifestsRepoConfig, PublishConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "kubectl-mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    manifests_repo: Some(KrewManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "krew-index".to_string(),
                    }),
                    short_description: Some("A great plugin".to_string()),
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

        assert!(publish_to_krew(&ctx, "kubectl-mytool", &log).is_ok());
    }

    #[test]
    fn test_publish_to_krew_missing_config() {
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

        assert!(publish_to_krew(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_krew_missing_manifests_repo() {
        use anodize_core::config::{Config, CrateConfig, KrewConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    manifests_repo: None, // Missing
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

        assert!(publish_to_krew(&ctx, "mytool", &log).is_err());
    }
}
