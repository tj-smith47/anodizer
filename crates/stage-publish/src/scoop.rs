use anodize_core::context::Context;
use anyhow::{Context as _, Result};

use crate::util::{find_windows_artifact, run_cmd, run_cmd_in};

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
    let (url, hash) = if let Some(found) = find_windows_artifact(ctx, crate_name) {
        found
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
        format!("https://github.com/{}/{}.git", bucket.owner, bucket.name)
    };

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    run_cmd(
        "git",
        &[
            "clone",
            "--depth=1",
            &clone_url,
            &repo_path.to_string_lossy(),
        ],
        "scoop: git clone",
    )?;

    let manifest_path = repo_path.join(format!("{}.json", crate_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    eprintln!(
        "[publish] wrote Scoop manifest: {}",
        manifest_path.display()
    );

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
        assert_eq!(arch_64["bin"], "anodize");

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
            json["architecture"]["64bit"]["bin"], "my-special-cli",
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
        // https://github.com/<name>/<name>/releases/download/v$version/<name>-$version-windows-amd64.zip
        assert!(auto_url.starts_with(
            "https://github.com/release-tool/release-tool/releases/download/v$version/"
        ));
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
        assert_eq!(arch64["bin"], "myapp");
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
        assert!(json["autoupdate"]["architecture"]["64bit"]["url"]
            .as_str()
            .unwrap()
            .contains("$version"));
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
}
