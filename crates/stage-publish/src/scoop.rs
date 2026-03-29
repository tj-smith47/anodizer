use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Optional extended fields for manifest generation.
#[derive(Default)]
pub struct ManifestOptions<'a> {
    /// Explicit homepage URL.  Falls back to the GitHub release URL when available.
    pub homepage: Option<&'a str>,
    /// GitHub owner/name for default homepage fallback (e.g. "owner/repo").
    pub github_slug: Option<String>,
    /// Data paths persisted between updates.
    pub persist: Option<&'a [String]>,
    /// Application dependencies.
    pub depends: Option<&'a [String]>,
    /// Commands to run before installation.
    pub pre_install: Option<&'a [String]>,
    /// Commands to run after installation.
    pub post_install: Option<&'a [String]>,
    /// Start menu shortcuts.
    pub shortcuts: Option<&'a [Vec<String>]>,
}

/// Generate a Scoop JSON manifest string for a Windows binary.
pub fn generate_manifest(
    name: &str,
    version: &str,
    url: &str,
    hash: &str,
    description: &str,
    license: &str,
) -> String {
    generate_manifest_with_opts(
        name,
        version,
        url,
        hash,
        description,
        license,
        &ManifestOptions::default(),
    )
}

/// Generate a Scoop JSON manifest string with extended options.
pub fn generate_manifest_with_opts(
    name: &str,
    version: &str,
    url: &str,
    hash: &str,
    description: &str,
    license: &str,
    opts: &ManifestOptions<'_>,
) -> String {
    // Homepage: explicit > GitHub owner/repo > bare name fallback.
    let default_homepage = opts
        .github_slug
        .as_deref()
        .map(|slug| format!("https://github.com/{}", slug))
        .unwrap_or_else(|| format!("https://github.com/{}", name));
    let homepage = opts.homepage.unwrap_or(&default_homepage);

    // Scoop bin entry should include .exe for Windows.
    let bin_name = format!("{}.exe", name);

    // Autoupdate URL uses GitHub slug if available.
    let autoupdate_prefix = opts.github_slug.as_deref().unwrap_or(name);

    let mut manifest = serde_json::json!({
        "version": version,
        "description": description,
        "homepage": homepage,
        "license": license,
        "architecture": {
            "64bit": {
                "url": url,
                "hash": hash,
                "bin": bin_name
            }
        },
        "checkver": "github",
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": format!(
                        "https://github.com/{}/releases/download/v$version/{}-$version-windows-amd64.zip",
                        autoupdate_prefix, name
                    )
                }
            }
        }
    });

    // Add optional array fields when present.
    let obj = manifest.as_object_mut().expect("manifest is an object");

    if let Some(persist) = opts.persist {
        obj.insert("persist".to_string(), serde_json::json!(persist));
    }
    if let Some(depends) = opts.depends {
        obj.insert("depends".to_string(), serde_json::json!(depends));
    }
    if let Some(pre_install) = opts.pre_install {
        obj.insert("pre_install".to_string(), serde_json::json!(pre_install));
    }
    if let Some(post_install) = opts.post_install {
        obj.insert("post_install".to_string(), serde_json::json!(post_install));
    }
    if let Some(shortcuts) = opts.shortcuts {
        obj.insert("shortcuts".to_string(), serde_json::json!(shortcuts));
    }

    // SAFETY: The manifest is a serde_json::Value constructed from string
    // literals and function parameters; serialisation to JSON is infallible.
    serde_json::to_string_pretty(&manifest).expect("scoop: serialize manifest")
}

// ---------------------------------------------------------------------------
// publish_to_scoop
// ---------------------------------------------------------------------------

pub fn publish_to_scoop(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "scoop")?;

    let scoop_cfg = publish
        .scoop
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no scoop config for '{}'", crate_name))?;

    // Check skip_upload before doing any work.
    if crate::homebrew::should_skip_upload(scoop_cfg.skip_upload.as_deref(), ctx) {
        log.status(&format!(
            "scoop: skipping upload for '{}' (skip_upload={})",
            crate_name,
            scoop_cfg.skip_upload.as_deref().unwrap_or("")
        ));
        return Ok(());
    }

    let bucket = scoop_cfg
        .bucket
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scoop: no bucket config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Scoop bucket {}/{} for '{}'",
            bucket.owner, bucket.name, crate_name
        ));
        return Ok(());
    }

    let version = ctx.version();

    let description = scoop_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());

    let license = scoop_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());

    // Find the windows-amd64 Archive artifact.
    let (url, hash) = util::require_windows_artifact(ctx, crate_name, "scoop")?;

    // Derive GitHub slug (owner/repo) for homepage fallback.
    let github_slug = _crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|gh| format!("{}/{}", gh.owner, gh.name));

    let opts = ManifestOptions {
        homepage: scoop_cfg.homepage.as_deref(),
        github_slug,
        persist: scoop_cfg.persist.as_deref(),
        depends: scoop_cfg.depends.as_deref(),
        pre_install: scoop_cfg.pre_install.as_deref(),
        post_install: scoop_cfg.post_install.as_deref(),
        shortcuts: scoop_cfg.shortcuts.as_deref(),
    };

    let manifest = generate_manifest_with_opts(
        crate_name,
        &version,
        &url,
        &hash,
        &description,
        &license,
        &opts,
    );

    // Clone bucket repo, write manifest, commit, push.
    let token = util::resolve_token(ctx, Some("SCOOP_BUCKET_TOKEN"));
    let repo_url = format!("https://github.com/{}/{}.git", bucket.owner, bucket.name);

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo_with_auth(&repo_url, token.as_deref(), repo_path, "scoop", log)?;

    let manifest_path = repo_path.join(format!("{}.json", crate_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    log.status(&format!(
        "wrote Scoop manifest: {}",
        manifest_path.display()
    ));

    let manifest_lossy = manifest_path.to_string_lossy();
    util::commit_and_push(
        repo_path,
        &[&manifest_lossy],
        &format!("chore: update {} manifest to {}", crate_name, version),
        None,
        "scoop",
    )?;

    log.status(&format!(
        "Scoop bucket {}/{} updated for '{}'",
        bucket.owner, bucket.name, crate_name
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
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
        assert_eq!(
            json["architecture"]["64bit"]["url"],
            "https://example.com/my-tool-2.1.0-windows-amd64.zip"
        );
    }

    #[test]
    fn test_publish_to_scoop_dry_run() {
        use anodize_core::config::{BucketConfig, Config, CrateConfig, PublishConfig, ScoopConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

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
        let log = StageLogger::new("publish", Verbosity::Normal);

        // dry-run should succeed without any network/git calls
        assert!(publish_to_scoop(&ctx, "cfgd", &log).is_ok());
    }

    // -----------------------------------------------------------------------
    // Deep integration tests: verify manifest JSON structure
    // -----------------------------------------------------------------------

    #[test]
    fn test_integration_manifest_complete_json_structure() {
        let manifest = generate_manifest(
            "anodize",
            "3.2.1",
            "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-windows-amd64.zip",
            "aabbccdd1122334455667788",
            "Release automation for Rust projects",
            "Apache-2.0",
        );

        // Parse the manifest as JSON
        let json: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest should be valid JSON");

        // Verify top-level fields exist and have correct values
        assert_eq!(json["version"], "3.2.1");
        assert_eq!(json["description"], "Release automation for Rust projects");
        assert_eq!(json["homepage"], "https://github.com/anodize");
        assert_eq!(json["license"], "Apache-2.0");

        // Verify architecture.64bit structure
        let arch_64 = &json["architecture"]["64bit"];
        assert!(
            arch_64.is_object(),
            "architecture.64bit should be an object"
        );
        assert_eq!(
            arch_64["url"],
            "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-windows-amd64.zip"
        );
        assert_eq!(arch_64["hash"], "aabbccdd1122334455667788");
        assert_eq!(arch_64["bin"], "anodize.exe");

        // Verify checkver field
        assert_eq!(json["checkver"], "github");

        // Verify autoupdate structure
        let autoupdate = &json["autoupdate"];
        assert!(autoupdate.is_object(), "autoupdate should be an object");
        let auto_64 = &autoupdate["architecture"]["64bit"];
        assert!(
            auto_64.is_object(),
            "autoupdate.architecture.64bit should be an object"
        );
        let auto_url = auto_64["url"].as_str().unwrap();
        assert!(
            auto_url.contains("anodize"),
            "autoupdate URL should contain the app name"
        );
        assert!(
            auto_url.contains("$version"),
            "autoupdate URL should contain $version placeholder"
        );
    }

    #[test]
    fn test_integration_manifest_is_valid_pretty_json() {
        let manifest = generate_manifest(
            "my-tool",
            "1.5.0",
            "https://example.com/my-tool-1.5.0-windows-amd64.zip",
            "deadbeefcafebabe",
            "A useful tool",
            "MIT",
        );

        // Verify it is pretty-printed (has newlines and indentation)
        assert!(manifest.contains('\n'), "should be pretty-printed");
        assert!(manifest.contains("  "), "should have indentation");

        // Verify it can be re-parsed
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify all expected top-level keys
        let obj = json.as_object().unwrap();
        let keys: Vec<&String> = obj.keys().collect();
        assert!(
            keys.iter().any(|k| k.as_str() == "version"),
            "should have version key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "description"),
            "should have description key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "homepage"),
            "should have homepage key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "license"),
            "should have license key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "architecture"),
            "should have architecture key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "checkver"),
            "should have checkver key"
        );
        assert!(
            keys.iter().any(|k| k.as_str() == "autoupdate"),
            "should have autoupdate key"
        );
    }

    #[test]
    fn test_integration_manifest_special_characters_in_description() {
        let manifest = generate_manifest(
            "json-tool",
            "1.0.0",
            "https://example.com/tool.zip",
            "hash123",
            "A tool for \"parsing\" JSON & XML <data>",
            "MIT",
        );

        // Even with special characters, should produce valid JSON
        let json: serde_json::Value = serde_json::from_str(&manifest)
            .expect("manifest with special chars should still be valid JSON");
        assert_eq!(
            json["description"],
            "A tool for \"parsing\" JSON & XML <data>"
        );
    }

    #[test]
    fn test_integration_manifest_bin_matches_name() {
        // Verify that the bin field in the manifest matches the name parameter
        let manifest = generate_manifest(
            "my-special-cli",
            "0.1.0",
            "https://example.com/cli.zip",
            "abc",
            "desc",
            "MIT",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(
            json["architecture"]["64bit"]["bin"], "my-special-cli.exe",
            "bin should match the tool name"
        );
    }

    #[test]
    fn test_integration_manifest_autoupdate_url_format() {
        let manifest = generate_manifest(
            "release-tool",
            "5.0.0",
            "https://example.com/release-tool-5.0.0-windows-amd64.zip",
            "hash",
            "desc",
            "MIT",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let auto_url = json["autoupdate"]["architecture"]["64bit"]["url"]
            .as_str()
            .unwrap();

        // The autoupdate URL should follow the pattern:
        // https://github.com/<name>/releases/download/v$version/<name>-$version-windows-amd64.zip
        // When github_slug is set, it uses owner/repo instead of bare name.
        assert!(
            auto_url.starts_with("https://github.com/release-tool/releases/download/v$version/")
        );
        assert!(auto_url.ends_with("-windows-amd64.zip"));
        assert!(auto_url.contains("release-tool-$version-"));
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_scoop_manifest_architecture_structure() {
        let manifest = generate_manifest(
            "myapp",
            "1.0.0",
            "https://example.com/myapp-1.0.0-windows-amd64.zip",
            "deadbeef",
            "My application",
            "Apache-2.0",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify architecture.64bit has all expected fields
        let arch64 = &json["architecture"]["64bit"];
        assert_eq!(
            arch64["url"],
            "https://example.com/myapp-1.0.0-windows-amd64.zip"
        );
        assert_eq!(arch64["hash"], "deadbeef");
        assert_eq!(arch64["bin"], "myapp.exe");
    }

    #[test]
    fn test_scoop_manifest_checkver_and_autoupdate() {
        let manifest = generate_manifest(
            "mytool",
            "2.0.0",
            "https://example.com/mytool.zip",
            "abc",
            "desc",
            "MIT",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["checkver"], "github");
        assert!(json["autoupdate"].is_object());
        assert!(
            json["autoupdate"]["architecture"]["64bit"]["url"]
                .as_str()
                .unwrap()
                .contains("$version")
        );
    }

    #[test]
    fn test_scoop_manifest_homepage_derived_from_name() {
        let manifest = generate_manifest(
            "my-tool",
            "1.0.0",
            "https://example.com/t.zip",
            "hash",
            "desc",
            "MIT",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://github.com/my-tool");
    }

    // -----------------------------------------------------------------------
    // New fields: homepage, persist, depends, pre/post_install, shortcuts
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_custom_homepage() {
        let opts = ManifestOptions {
            homepage: Some("https://example.com/mytool"),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://example.com/mytool");
    }

    #[test]
    fn test_manifest_homepage_fallback() {
        let manifest = generate_manifest(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://github.com/mytool");
    }

    #[test]
    fn test_manifest_persist() {
        let persist = vec!["data".to_string(), "config.ini".to_string()];
        let opts = ManifestOptions {
            persist: Some(&persist),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["persist"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "data");
        assert_eq!(arr[1], "config.ini");
    }

    #[test]
    fn test_manifest_depends() {
        let depends = vec!["git".to_string(), "7zip".to_string()];
        let opts = ManifestOptions {
            depends: Some(&depends),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["depends"].as_array().unwrap();
        assert_eq!(arr, &["git", "7zip"]);
    }

    #[test]
    fn test_manifest_pre_install() {
        let pre = vec!["Write-Host 'Installing...'".to_string()];
        let opts = ManifestOptions {
            pre_install: Some(&pre),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["pre_install"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Write-Host 'Installing...'");
    }

    #[test]
    fn test_manifest_post_install() {
        let post = vec!["Write-Host 'Done!'".to_string()];
        let opts = ManifestOptions {
            post_install: Some(&post),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["post_install"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], "Write-Host 'Done!'");
    }

    #[test]
    fn test_manifest_shortcuts() {
        let shortcuts = vec![
            vec!["myapp.exe".to_string(), "My App".to_string()],
            vec![
                "myapp.exe".to_string(),
                "My App CLI".to_string(),
                "--cli".to_string(),
            ],
        ];
        let opts = ManifestOptions {
            shortcuts: Some(&shortcuts),
            ..Default::default()
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let arr = json["shortcuts"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0][0], "myapp.exe");
        assert_eq!(arr[0][1], "My App");
        assert_eq!(arr[1][2], "--cli");
    }

    #[test]
    fn test_manifest_no_optional_fields_when_not_set() {
        let manifest = generate_manifest(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert!(json.get("persist").is_none());
        assert!(json.get("depends").is_none());
        assert!(json.get("pre_install").is_none());
        assert!(json.get("post_install").is_none());
        assert!(json.get("shortcuts").is_none());
    }

    #[test]
    fn test_manifest_all_new_fields_together() {
        let persist = vec!["data".to_string()];
        let depends = vec!["git".to_string()];
        let pre = vec!["echo pre".to_string()];
        let post = vec!["echo post".to_string()];
        let shortcuts = vec![vec!["app.exe".to_string(), "App".to_string()]];
        let opts = ManifestOptions {
            homepage: Some("https://example.com"),
            github_slug: None,
            persist: Some(&persist),
            depends: Some(&depends),
            pre_install: Some(&pre),
            post_install: Some(&post),
            shortcuts: Some(&shortcuts),
        };
        let manifest = generate_manifest_with_opts(
            "mytool",
            "1.0.0",
            "https://example.com/a.zip",
            "abc",
            "desc",
            "MIT",
            &opts,
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://example.com");
        assert!(json["persist"].is_array());
        assert!(json["depends"].is_array());
        assert!(json["pre_install"].is_array());
        assert!(json["post_install"].is_array());
        assert!(json["shortcuts"].is_array());
    }

    // -----------------------------------------------------------------------
    // skip_upload tests (reuses should_skip_upload from homebrew)
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_scoop_skip_upload_true() {
        use anodize_core::config::{BucketConfig, Config, CrateConfig, PublishConfig, ScoopConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let config = Config {
            crates: vec![CrateConfig {
                name: "skipped".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    scoop: Some(ScoopConfig {
                        bucket: Some(BucketConfig {
                            owner: "myorg".to_string(),
                            name: "scoop-bucket".to_string(),
                        }),
                        skip_upload: Some("true".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let ctx = Context::new(config, ContextOptions::default());
        let log = StageLogger::new("publish", Verbosity::Normal);
        assert!(publish_to_scoop(&ctx, "skipped", &log).is_ok());
    }

    #[test]
    fn test_publish_to_scoop_skip_upload_auto_prerelease() {
        use anodize_core::config::{BucketConfig, Config, CrateConfig, PublishConfig, ScoopConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let config = Config {
            crates: vec![CrateConfig {
                name: "pre".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    scoop: Some(ScoopConfig {
                        bucket: Some(BucketConfig {
                            owner: "myorg".to_string(),
                            name: "scoop-bucket".to_string(),
                        }),
                        skip_upload: Some("auto".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Prerelease", "alpha.1");
        let log = StageLogger::new("publish", Verbosity::Normal);
        assert!(publish_to_scoop(&ctx, "pre", &log).is_ok());
    }
}
