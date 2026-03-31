use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::util::{self, OsArtifact};

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

    // SAFETY: The manifest struct is composed entirely of Strings and Vecs;
    // YAML serialisation is infallible for these types.
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
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "krew")?;

    let krew_cfg = publish
        .krew
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("krew: no krew config for '{}'", crate_name))?;

    // Check skip_upload before doing any work.
    if crate::homebrew::should_skip_upload(krew_cfg.skip_upload.as_deref(), ctx) {
        log.status(&format!(
            "krew: skipping upload for '{}' (skip_upload={})",
            crate_name,
            krew_cfg.skip_upload.as_deref().unwrap_or("")
        ));
        return Ok(());
    }

    // Resolve repository config: prefer `repository` over legacy `manifests_repo`.
    let (repo_owner, repo_name) = crate::util::resolve_repo_owner_name(
        krew_cfg.repository.as_ref(),
        krew_cfg.manifests_repo.as_ref().map(|r| r.owner.as_str()),
        krew_cfg.manifests_repo.as_ref().map(|r| r.name.as_str()),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "krew: no repository/manifests_repo config for '{}'",
            crate_name
        )
    })?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would submit Krew plugin manifest for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(());
    }

    let version = ctx.version();

    let description_raw = krew_cfg.description.as_deref().unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let short_description_raw = krew_cfg.short_description.as_deref().unwrap_or(crate_name);
    let short_description = ctx
        .render_template(short_description_raw)
        .unwrap_or_else(|_| short_description_raw.to_string());
    // Derive GitHub slug (owner/repo) for homepage fallback, consistent with homebrew.
    let github_slug = _crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));
    let homepage_raw = krew_cfg.homepage.clone().unwrap_or_else(|| {
        github_slug
            .as_deref()
            .map(|slug| format!("https://github.com/{}", slug))
            .unwrap_or_else(|| format!("https://github.com/{}/{}", repo_owner, crate_name))
    });
    let homepage = ctx
        .render_template(&homepage_raw)
        .unwrap_or_else(|_| homepage_raw.clone());
    let caveats = krew_cfg.caveats.clone().unwrap_or_default();

    // Find artifacts across all platforms, applying IDs filter.
    let ids_filter = krew_cfg.ids.as_deref();
    let all_artifacts = util::find_all_platform_artifacts_filtered(ctx, crate_name, ids_filter);

    let url_template = krew_cfg.url_template.as_deref();

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
                repo_owner, crate_name, version
            ),
            sha256: String::new(),
            bin: crate_name.to_string(),
        }]
    } else {
        let mut plats = artifacts_to_platforms(&all_artifacts, crate_name);
        if let Some(tmpl) = url_template {
            for p in &mut plats {
                p.url = util::render_url_template(tmpl, crate_name, &version, &p.arch, &p.os);
            }
        }
        plats
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

    // Use name override if set; render through template engine.
    let plugin_name_raw = krew_cfg.name.as_deref().unwrap_or(crate_name);
    let plugin_name_rendered = ctx
        .render_template(plugin_name_raw)
        .unwrap_or_else(|_| plugin_name_raw.to_string());
    let plugin_name = plugin_name_rendered.as_str();

    // Clone the krew-index fork, write the plugin manifest, commit, push.
    let token = util::resolve_repo_token(ctx, krew_cfg.repository.as_ref(), None);

    let tmp_dir = tempfile::tempdir().context("krew: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        krew_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "krew",
        log,
    )?;

    // Write plugin manifest under plugins/<name>.yaml.
    let plugins_dir = repo_path.join("plugins");
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("krew: create plugins dir {}", plugins_dir.display()))?;

    let manifest_file = plugins_dir.join(format!("{}.yaml", plugin_name));
    std::fs::write(&manifest_file, &manifest)
        .with_context(|| format!("krew: write manifest {}", manifest_file.display()))?;

    log.status(&format!(
        "wrote Krew plugin manifest: {}",
        manifest_file.display()
    ));

    let commit_msg = crate::homebrew::render_commit_msg(
        krew_cfg.commit_msg_template.as_deref(),
        plugin_name,
        &version,
        "plugin",
    );
    let branch_name = format!("{}-v{}", plugin_name, version);
    let commit_opts = util::resolve_commit_opts(krew_cfg.commit_author.as_ref(), None, None);
    // Always create a versioned branch for Krew PRs.
    let branch = Some(branch_name.as_str());
    util::commit_and_push_with_opts(repo_path, &["."], &commit_msg, branch, "krew", &commit_opts)?;

    log.status(&format!(
        "Krew manifest pushed to {}/{} branch '{}'",
        repo_owner, repo_name, branch_name
    ));

    // Submit a PR.  When `repository.pull_request` is configured, use
    // the unified PR helper (which respects `base`, `draft`, `body`).
    // Otherwise fall back to the legacy `upstream_repo` field.
    let has_pr_config = krew_cfg
        .repository
        .as_ref()
        .and_then(|r| r.pull_request.as_ref())
        .and_then(|pr| pr.enabled)
        .unwrap_or(false);

    if has_pr_config {
        util::maybe_submit_pr(
            repo_path,
            krew_cfg.repository.as_ref(),
            &repo_owner,
            &repo_name,
            &branch_name,
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\nAutomatically submitted by anodize.",
                crate_name, version
            ),
            "krew",
            log,
        );
    } else {
        // Legacy path: always submit a PR using upstream_repo or own repo.
        let upstream_slug = if let Some(ref u) = krew_cfg.upstream_repo {
            format!("{}/{}", u.owner, u.name)
        } else {
            format!("{}/{}", repo_owner, repo_name)
        };

        util::submit_pr_via_gh(
            repo_path,
            &upstream_slug,
            &format!("{}:{}", repo_owner, branch_name),
            &format!("Add/update {} plugin to v{}", crate_name, version),
            &format!(
                "## Plugin\n- **Name**: {}\n- **Version**: v{}\n\nAutomatically submitted by anodize.",
                crate_name, version
            ),
            "krew",
            log,
        );
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
