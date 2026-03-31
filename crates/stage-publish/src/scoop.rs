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
    /// Binary names (without `.exe` extension) to use in the `bin` field.
    /// When set, these are used instead of deriving from the manifest name.
    /// Multiple entries produce a JSON array in the `bin` field.
    pub bin: Option<&'a [String]>,
}

/// A single architecture entry for the Scoop manifest.
pub struct ArchEntry {
    /// Scoop architecture key: "64bit", "32bit", or "arm64".
    pub scoop_arch: String,
    pub url: String,
    pub hash: String,
    /// When the archive wraps contents in a top-level directory, this holds that
    /// directory name.  Bin entries will be prefixed with it (e.g. `dir\bin.exe`).
    pub wrap_in_directory: Option<String>,
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
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: url.to_string(),
        hash: hash.to_string(),
        wrap_in_directory: None,
    }];
    generate_manifest_with_opts(
        name,
        version,
        &entries,
        description,
        license,
        &ManifestOptions::default(),
    )
}

/// Generate a Scoop JSON manifest string with extended options.
///
/// Accepts multiple architecture entries. Each entry maps to a key in
/// the `architecture` block: `64bit`, `32bit`, or `arm64`.
pub fn generate_manifest_with_opts(
    name: &str,
    version: &str,
    arch_entries: &[ArchEntry],
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

    // Scoop bin entry: use explicit binary names when provided, otherwise
    // derive from the manifest name. Append `.exe` only if not already present.
    // GoReleaser uses artifact metadata binary names as-is (they already include .exe).
    let ensure_exe = |b: &str| -> String {
        if b.ends_with(".exe") {
            b.to_string()
        } else {
            format!("{}.exe", b)
        }
    };

    // Compute bin value for a given wrap_in_directory prefix.
    // When wrap_in_directory is set, each bin entry becomes a pair:
    //   ["wrap_dir\\binary.exe", "alias"]
    // where alias is the binary name without the .exe extension.
    // This matches GoReleaser's WrappedIn handling for Scoop manifests.
    let make_bin_value = |wrap_dir: Option<&str>| -> serde_json::Value {
        let raw_bins: Vec<String> = match opts.bin {
            Some(bins) if !bins.is_empty() => bins.iter().map(|b| ensure_exe(b)).collect(),
            _ => vec![ensure_exe(name)],
        };
        match wrap_dir {
            Some(dir) if !dir.is_empty() => {
                let pairs: Vec<serde_json::Value> = raw_bins
                    .iter()
                    .map(|exe| {
                        let alias = exe.strip_suffix(".exe").unwrap_or(exe);
                        serde_json::json!([format!("{}\\{}", dir, exe), alias])
                    })
                    .collect();
                serde_json::json!(pairs)
            }
            _ => {
                // Single binary: plain string; multiple: array.
                // This matches the original Scoop manifest format.
                if raw_bins.len() == 1 {
                    serde_json::json!(raw_bins[0])
                } else {
                    serde_json::json!(raw_bins)
                }
            }
        }
    };

    // Build the architecture block from entries.
    let mut arch_obj = serde_json::Map::new();
    for entry in arch_entries {
        let bin_value = make_bin_value(entry.wrap_in_directory.as_deref());
        arch_obj.insert(
            entry.scoop_arch.clone(),
            serde_json::json!({
                "url": entry.url,
                "hash": entry.hash,
                "bin": bin_value
            }),
        );
    }

    let mut manifest = serde_json::json!({
        "version": version,
        "description": description,
        "homepage": homepage,
        "license": license,
        "architecture": arch_obj
    });

    // Only include checkver + autoupdate when a valid GitHub slug (owner/repo) is
    // available.  Without a slug the URL would be broken (bare crate name used as
    // the GitHub path component), so we omit both fields entirely.
    if let Some(slug) = opts.github_slug.as_deref() {
        let obj = manifest.as_object_mut().expect("manifest is an object");
        obj.insert("checkver".to_string(), serde_json::json!("github"));

        // Build autoupdate architecture block mirroring the main architecture entries.
        let mut auto_arch = serde_json::Map::new();
        for entry in arch_entries {
            let arch_suffix = match entry.scoop_arch.as_str() {
                "64bit" => "amd64",
                "32bit" => "386",
                "arm64" => "arm64",
                other => other,
            };
            auto_arch.insert(
                entry.scoop_arch.clone(),
                serde_json::json!({
                    "url": format!(
                        "https://github.com/{}/releases/download/v$version/{}-$version-windows-{}.zip",
                        slug, name, arch_suffix
                    )
                }),
            );
        }

        obj.insert(
            "autoupdate".to_string(),
            serde_json::json!({
                "architecture": auto_arch
            }),
        );
    }

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

    // Resolve repository config: prefer `repository` over legacy `bucket`.
    let (repo_owner, repo_name) = crate::util::resolve_repo_owner_name(
        scoop_cfg.repository.as_ref(),
        scoop_cfg.bucket.as_ref().map(|b| b.owner.as_str()),
        scoop_cfg.bucket.as_ref().map(|b| b.name.as_str()),
    )
    .ok_or_else(|| anyhow::anyhow!("scoop: no repository/bucket config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Scoop bucket {}/{} for '{}'",
            repo_owner, repo_name, crate_name
        ));
        return Ok(());
    }

    let version = ctx.version();

    let description_raw = scoop_cfg.description.as_deref().unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());

    let license = scoop_cfg
        .license
        .clone()
        .unwrap_or_else(|| "MIT".to_string());

    // Use name override if set, otherwise crate name; render through template engine.
    let manifest_name_raw = scoop_cfg.name.as_deref().unwrap_or(crate_name);
    let manifest_name_rendered = ctx
        .render_template(manifest_name_raw)
        .unwrap_or_else(|_| manifest_name_raw.to_string());
    let manifest_name = manifest_name_rendered.as_str();

    // Find all Windows Archive artifacts, applying IDs filter when configured.
    let ids_filter = scoop_cfg.ids.as_deref();
    let url_template = scoop_cfg.url_template.as_deref();

    let artifact_kind = util::resolve_artifact_kind(scoop_cfg.use_artifact.as_deref());
    let all_artifacts = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);

    let arch_entries: Vec<ArchEntry> = all_artifacts
        .into_iter()
        .filter(|a| {
            // Only windows artifacts.
            a.target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains("windows"))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("windows")
        })
        .filter(|a| {
            // Apply IDs filter if configured.
            if let Some(ids) = ids_filter {
                a.metadata
                    .get("id")
                    .map(|id| ids.iter().any(|i| i == id))
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .map(|a| {
            let target = a.target.as_deref().unwrap_or("");
            let (_, raw_arch) = anodize_core::target::map_target(target);

            // Map architecture to Scoop keys.
            let scoop_arch = match raw_arch.as_str() {
                "amd64" => "64bit",
                "386" => "32bit",
                "arm64" => "arm64",
                _ => "64bit",
            };

            // Resolve download URL: use url_template if set, otherwise artifact metadata.
            let url = if let Some(tmpl) = url_template {
                util::render_url_template(tmpl, manifest_name, &version, &raw_arch, "windows")
            } else {
                a.metadata
                    .get("url")
                    .cloned()
                    .unwrap_or_else(|| a.path.to_string_lossy().into_owned())
            };

            let hash = a.metadata.get("sha256").cloned().unwrap_or_default();
            let wrap_in_directory = a.metadata.get("wrap_in_directory").cloned();

            ArchEntry {
                scoop_arch: scoop_arch.to_string(),
                url,
                hash,
                wrap_in_directory,
            }
        })
        .collect();

    if arch_entries.is_empty() {
        anyhow::bail!(
            "scoop: no Windows archive artifact found for crate '{}'",
            crate_name
        );
    }

    // Collect binary names from artifact metadata.  The archive stage stores
    // the binary name in the `"binary"` metadata key.  We deduplicate to get
    // a unique set of binary names across all architecture variants.
    let bin_names: Vec<String> = {
        let mut names = Vec::new();
        let artifact_kind = util::resolve_artifact_kind(scoop_cfg.use_artifact.as_deref());
        let all_win = ctx.artifacts.by_kind_and_crate(artifact_kind, crate_name);
        for a in &all_win {
            let is_win = a
                .target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains("windows"))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("windows");
            if !is_win {
                continue;
            }
            if let Some(bin) = a.metadata.get("binary")
                && !names.contains(bin)
            {
                names.push(bin.clone());
            }
        }
        names
    };
    let bin_names_ref: Option<&[String]> = if bin_names.is_empty() {
        None
    } else {
        Some(&bin_names)
    };

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
        bin: bin_names_ref,
    };

    let manifest = generate_manifest_with_opts(
        manifest_name,
        &version,
        &arch_entries,
        &description,
        &license,
        &opts,
    );

    // Clone bucket repo, write manifest, commit, push.
    let token = util::resolve_repo_token(
        ctx,
        scoop_cfg.repository.as_ref(),
        Some("SCOOP_BUCKET_TOKEN"),
    );

    let tmp_dir = tempfile::tempdir().context("scoop: create temp dir")?;
    let repo_path = tmp_dir.path();

    util::clone_repo(
        scoop_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "scoop",
        log,
    )?;

    // Place manifest in optional subdirectory.
    let manifest_dir = if let Some(dir) = scoop_cfg.directory.as_deref() {
        let d = repo_path.join(dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("scoop: create directory {}", d.display()))?;
        d
    } else {
        repo_path.to_path_buf()
    };

    let manifest_path = manifest_dir.join(format!("{}.json", manifest_name));
    std::fs::write(&manifest_path, &manifest)
        .with_context(|| format!("scoop: write manifest {}", manifest_path.display()))?;

    log.status(&format!(
        "wrote Scoop manifest: {}",
        manifest_path.display()
    ));

    // Render commit message from template or use default.
    let commit_msg = crate::homebrew::render_commit_msg(
        scoop_cfg.commit_msg_template.as_deref(),
        manifest_name,
        &version,
        "manifest",
    );

    let manifest_lossy = manifest_path.to_string_lossy();
    let commit_opts = util::resolve_commit_opts(
        scoop_cfg.commit_author.as_ref(),
        scoop_cfg.commit_author_name.as_deref(),
        scoop_cfg.commit_author_email.as_deref(),
    );
    let branch = util::resolve_branch(scoop_cfg.repository.as_ref());
    util::commit_and_push_with_opts(
        repo_path,
        &[&manifest_lossy],
        &commit_msg,
        branch,
        "scoop",
        &commit_opts,
    )?;

    log.status(&format!(
        "Scoop bucket {}/{} updated for '{}'",
        repo_owner, repo_name, crate_name
    ));

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.unwrap_or("main");
    util::maybe_submit_pr(
        repo_path,
        scoop_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        pr_branch,
        &format!("Update {} manifest to {}", manifest_name, version),
        &format!(
            "## Manifest\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodize.",
            manifest_name, version
        ),
        "scoop",
        log,
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

    /// Helper to build a single 64bit ArchEntry for test convenience.
    fn arch_64(url: &str, hash: &str) -> Vec<ArchEntry> {
        vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: url.to_string(),
            hash: hash.to_string(),
            wrap_in_directory: None,
        }]
    }

    #[test]
    fn test_integration_manifest_complete_json_structure() {
        let opts = ManifestOptions {
            github_slug: Some("tj-smith47/anodize".to_string()),
            ..Default::default()
        };
        let entries = arch_64(
            "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-windows-amd64.zip",
            "aabbccdd1122334455667788",
        );
        let manifest = generate_manifest_with_opts(
            "anodize",
            "3.2.1",
            &entries,
            "Release automation for Rust projects",
            "Apache-2.0",
            &opts,
        );

        // Parse the manifest as JSON
        let json: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest should be valid JSON");

        // Verify top-level fields exist and have correct values
        assert_eq!(json["version"], "3.2.1");
        assert_eq!(json["description"], "Release automation for Rust projects");
        assert_eq!(json["homepage"], "https://github.com/tj-smith47/anodize");
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
        // checkver and autoupdate are only present when github_slug is set
        assert!(
            !keys.iter().any(|k| k.as_str() == "checkver"),
            "should NOT have checkver key when github_slug is absent"
        );
        assert!(
            !keys.iter().any(|k| k.as_str() == "autoupdate"),
            "should NOT have autoupdate key when github_slug is absent"
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
        let opts = ManifestOptions {
            github_slug: Some("myorg/release-tool".to_string()),
            ..Default::default()
        };
        let entries = arch_64(
            "https://example.com/release-tool-5.0.0-windows-amd64.zip",
            "hash",
        );
        let manifest =
            generate_manifest_with_opts("release-tool", "5.0.0", &entries, "desc", "MIT", &opts);

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let auto_url = json["autoupdate"]["architecture"]["64bit"]["url"]
            .as_str()
            .unwrap();

        // The autoupdate URL should follow the pattern:
        // https://github.com/<owner>/<repo>/releases/download/v$version/<name>-$version-windows-amd64.zip
        assert!(
            auto_url
                .starts_with("https://github.com/myorg/release-tool/releases/download/v$version/")
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
    fn test_scoop_manifest_checkver_and_autoupdate_with_slug() {
        let opts = ManifestOptions {
            github_slug: Some("myorg/mytool".to_string()),
            ..Default::default()
        };
        let entries = arch_64("https://example.com/mytool.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "2.0.0", &entries, "desc", "MIT", &opts);

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
    fn test_scoop_manifest_no_checkver_autoupdate_without_slug() {
        let manifest = generate_manifest(
            "mytool",
            "2.0.0",
            "https://example.com/mytool.zip",
            "abc",
            "desc",
            "MIT",
        );

        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // Without github_slug, checkver and autoupdate should be absent
        assert!(
            json.get("checkver").is_none(),
            "checkver should be absent without github_slug"
        );
        assert!(
            json.get("autoupdate").is_none(),
            "autoupdate should be absent without github_slug"
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
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
            bin: None,
        };
        let entries = arch_64("https://example.com/a.zip", "abc");
        let manifest =
            generate_manifest_with_opts("mytool", "1.0.0", &entries, "desc", "MIT", &opts);
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["homepage"], "https://example.com");
        assert!(json["persist"].is_array());
        assert!(json["depends"].is_array());
        assert!(json["pre_install"].is_array());
        assert!(json["post_install"].is_array());
        assert!(json["shortcuts"].is_array());
    }

    // -----------------------------------------------------------------------
    // Multi-arch manifest tests (32bit + 64bit + arm64)
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_multi_arch_all_three() {
        let entries = vec![
            ArchEntry {
                scoop_arch: "64bit".to_string(),
                url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
                hash: "hash_amd64".to_string(),
                wrap_in_directory: None,
            },
            ArchEntry {
                scoop_arch: "32bit".to_string(),
                url: "https://example.com/app-1.0.0-windows-386.zip".to_string(),
                hash: "hash_386".to_string(),
                wrap_in_directory: None,
            },
            ArchEntry {
                scoop_arch: "arm64".to_string(),
                url: "https://example.com/app-1.0.0-windows-arm64.zip".to_string(),
                hash: "hash_arm64".to_string(),
                wrap_in_directory: None,
            },
        ];
        let opts = ManifestOptions {
            github_slug: Some("myorg/app".to_string()),
            ..Default::default()
        };
        let manifest =
            generate_manifest_with_opts("app", "1.0.0", &entries, "A multi-arch app", "MIT", &opts);
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();

        // Verify all three architecture blocks
        let arch = &json["architecture"];
        assert!(arch["64bit"].is_object(), "64bit block should exist");
        assert!(arch["32bit"].is_object(), "32bit block should exist");
        assert!(arch["arm64"].is_object(), "arm64 block should exist");

        // Verify URLs and hashes
        assert_eq!(
            arch["64bit"]["url"],
            "https://example.com/app-1.0.0-windows-amd64.zip"
        );
        assert_eq!(arch["64bit"]["hash"], "hash_amd64");
        assert_eq!(arch["64bit"]["bin"], "app.exe");

        assert_eq!(
            arch["32bit"]["url"],
            "https://example.com/app-1.0.0-windows-386.zip"
        );
        assert_eq!(arch["32bit"]["hash"], "hash_386");
        assert_eq!(arch["32bit"]["bin"], "app.exe");

        assert_eq!(
            arch["arm64"]["url"],
            "https://example.com/app-1.0.0-windows-arm64.zip"
        );
        assert_eq!(arch["arm64"]["hash"], "hash_arm64");
        assert_eq!(arch["arm64"]["bin"], "app.exe");

        // Verify autoupdate has all three architectures
        let auto = &json["autoupdate"]["architecture"];
        assert!(auto["64bit"].is_object(), "autoupdate.64bit should exist");
        assert!(auto["32bit"].is_object(), "autoupdate.32bit should exist");
        assert!(auto["arm64"].is_object(), "autoupdate.arm64 should exist");

        // Verify autoupdate URLs use correct arch suffixes
        let auto_64_url = auto["64bit"]["url"].as_str().unwrap();
        assert!(
            auto_64_url.contains("amd64"),
            "autoupdate 64bit should use amd64 suffix"
        );
        let auto_32_url = auto["32bit"]["url"].as_str().unwrap();
        assert!(
            auto_32_url.contains("386"),
            "autoupdate 32bit should use 386 suffix"
        );
        let auto_arm_url = auto["arm64"]["url"].as_str().unwrap();
        assert!(
            auto_arm_url.contains("arm64"),
            "autoupdate arm64 should use arm64 suffix"
        );
    }

    // -----------------------------------------------------------------------
    // wrap_in_directory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_wrap_in_directory_single_bin() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/app-1.0.0-windows-amd64.zip".to_string(),
            hash: "hash123".to_string(),
            wrap_in_directory: Some("app-1.0.0".to_string()),
        }];
        let manifest = generate_manifest_with_opts(
            "app",
            "1.0.0",
            &entries,
            "An app",
            "MIT",
            &ManifestOptions::default(),
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // With wrap_in_directory, single bin becomes a pair: ["dir\\bin.exe", "alias"]
        let bin = &json["architecture"]["64bit"]["bin"];
        assert!(bin.is_array(), "bin should be an array");
        let pair = &bin[0];
        assert!(pair.is_array(), "bin entry should be a [path, alias] pair");
        assert_eq!(pair[0], "app-1.0.0\\app.exe");
        assert_eq!(pair[1], "app");
    }

    #[test]
    fn test_manifest_wrap_in_directory_multiple_bins() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/suite-1.0.0.zip".to_string(),
            hash: "hash456".to_string(),
            wrap_in_directory: Some("suite-1.0.0".to_string()),
        }];
        let bins = vec!["cli".to_string(), "daemon".to_string()];
        let opts = ManifestOptions {
            bin: Some(&bins),
            ..Default::default()
        };
        let manifest =
            generate_manifest_with_opts("suite", "1.0.0", &entries, "A suite", "MIT", &opts);
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        let bin = &json["architecture"]["64bit"]["bin"];
        assert!(bin.is_array());
        assert_eq!(bin.as_array().unwrap().len(), 2);
        assert_eq!(bin[0][0], "suite-1.0.0\\cli.exe");
        assert_eq!(bin[0][1], "cli");
        assert_eq!(bin[1][0], "suite-1.0.0\\daemon.exe");
        assert_eq!(bin[1][1], "daemon");
    }

    #[test]
    fn test_manifest_no_wrap_preserves_simple_bin() {
        let entries = vec![ArchEntry {
            scoop_arch: "64bit".to_string(),
            url: "https://example.com/app.zip".to_string(),
            hash: "hash789".to_string(),
            wrap_in_directory: None,
        }];
        let manifest = generate_manifest_with_opts(
            "app",
            "1.0.0",
            &entries,
            "An app",
            "MIT",
            &ManifestOptions::default(),
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // Without wrap_in_directory, single bin is a plain string.
        assert_eq!(json["architecture"]["64bit"]["bin"], "app.exe");
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

    // -----------------------------------------------------------------------
    // Scoop manifest name override
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_name_override() {
        // When ScoopConfig.name is set, the manifest bin and filename should
        // use the override name.
        let manifest = generate_manifest(
            "custom-name",
            "1.0.0",
            "https://example.com/custom-name-1.0.0-windows-amd64.zip",
            "abc123",
            "A custom named tool",
            "MIT",
        );
        let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(json["architecture"]["64bit"]["bin"], "custom-name.exe");
    }

    // -----------------------------------------------------------------------
    // Scoop manifest directory placement (dry-run test)
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_scoop_dry_run_with_directory() {
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
                    directory: Some("bucket".to_string()),
                    description: Some("A tool".to_string()),
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

        // dry-run should succeed; the directory field is wired but no actual
        // file system operations happen in dry-run mode.
        assert!(publish_to_scoop(&ctx, "cfgd", &log).is_ok());
    }

    // -----------------------------------------------------------------------
    // Scoop commit message template (uses shared render_commit_msg)
    // -----------------------------------------------------------------------

    #[test]
    fn test_scoop_commit_msg_default() {
        let msg = crate::homebrew::render_commit_msg(None, "mytool", "1.2.3", "manifest");
        assert_eq!(msg, "chore: update mytool manifest to 1.2.3");
    }

    #[test]
    fn test_scoop_commit_msg_custom() {
        let msg = crate::homebrew::render_commit_msg(
            Some("scoop: bump {{ name }} to {{ version }}"),
            "mytool",
            "3.0.0",
            "manifest",
        );
        assert_eq!(msg, "scoop: bump mytool to 3.0.0");
    }
}
