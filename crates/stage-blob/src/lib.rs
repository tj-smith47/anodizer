use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::Artifact;
use anodize_core::config::BlobConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    S3,
    Gcs,
    AzBlob,
}

impl Provider {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "s3" => Ok(Provider::S3),
            "gs" | "gcs" => Ok(Provider::Gcs),
            "azblob" | "azure" => Ok(Provider::AzBlob),
            other => anyhow::bail!(
                "blobs: unknown provider '{}'. Valid providers are: s3, gcs, azblob",
                other
            ),
        }
    }

    /// Return the CLI binary name for this provider.
    pub fn cli_binary(&self) -> &'static str {
        match self {
            Provider::S3 => "aws",
            Provider::Gcs => "gsutil",
            Provider::AzBlob => "az",
        }
    }
}

// ---------------------------------------------------------------------------
// build_upload_command — construct the CLI invocation for each provider
// ---------------------------------------------------------------------------

/// Build the command to upload a single file to cloud storage.
pub fn build_upload_command(
    provider: Provider,
    config: &BlobConfig,
    local_path: &Path,
    remote_key: &str,
    rendered_bucket: &str,
    rendered_directory: &str,
) -> Vec<String> {
    match provider {
        Provider::S3 => build_s3_command(
            config,
            local_path,
            remote_key,
            rendered_bucket,
            rendered_directory,
        ),
        Provider::Gcs => build_gcs_command(
            config,
            local_path,
            remote_key,
            rendered_bucket,
            rendered_directory,
        ),
        Provider::AzBlob => build_azblob_command(
            config,
            local_path,
            remote_key,
            rendered_bucket,
            rendered_directory,
        ),
    }
}

fn build_s3_command(
    config: &BlobConfig,
    local_path: &Path,
    remote_key: &str,
    bucket: &str,
    directory: &str,
) -> Vec<String> {
    let remote_path = if directory.is_empty() {
        format!("s3://{}/{}", bucket, remote_key)
    } else {
        format!(
            "s3://{}/{}/{}",
            bucket,
            directory.trim_matches('/'),
            remote_key
        )
    };

    let mut args = vec![
        "aws".to_string(),
        "s3".to_string(),
        "cp".to_string(),
        local_path.to_string_lossy().into_owned(),
        remote_path,
    ];

    if let Some(ref region) = config.region {
        args.push("--region".to_string());
        args.push(region.clone());
    }
    if let Some(ref endpoint) = config.endpoint {
        args.push("--endpoint-url".to_string());
        args.push(endpoint.clone());
    }
    if let Some(ref acl) = config.acl {
        args.push("--acl".to_string());
        args.push(acl.clone());
    }
    if let Some(ref cache_control) = config.cache_control {
        args.push("--cache-control".to_string());
        args.push(cache_control.clone());
    }
    if let Some(ref content_disposition) = config.content_disposition {
        args.push("--content-disposition".to_string());
        args.push(content_disposition.clone());
    }
    if let Some(ref kms_key) = config.kms_key {
        args.push("--sse".to_string());
        args.push("aws:kms".to_string());
        args.push("--sse-kms-key-id".to_string());
        args.push(kms_key.clone());
    }
    if config.disable_ssl.unwrap_or(false) {
        args.push("--no-verify-ssl".to_string());
    }

    args
}

fn build_gcs_command(
    config: &BlobConfig,
    local_path: &Path,
    remote_key: &str,
    bucket: &str,
    directory: &str,
) -> Vec<String> {
    let remote_path = if directory.is_empty() {
        format!("gs://{}/{}", bucket, remote_key)
    } else {
        format!(
            "gs://{}/{}/{}",
            bucket,
            directory.trim_matches('/'),
            remote_key
        )
    };

    let mut args = vec!["gsutil".to_string()];

    // GCS supports setting headers via -h flag
    if let Some(ref cache_control) = config.cache_control {
        args.push("-h".to_string());
        args.push(format!("Cache-Control:{cache_control}"));
    }
    if let Some(ref content_disposition) = config.content_disposition {
        args.push("-h".to_string());
        args.push(format!("Content-Disposition:{content_disposition}"));
    }

    args.push("cp".to_string());

    // ACL can be set with -a flag on cp
    if let Some(ref acl) = config.acl {
        args.push("-a".to_string());
        args.push(acl.clone());
    }

    args.push(local_path.to_string_lossy().into_owned());
    args.push(remote_path);

    args
}

fn build_azblob_command(
    config: &BlobConfig,
    local_path: &Path,
    remote_key: &str,
    bucket: &str,
    directory: &str,
) -> Vec<String> {
    let blob_name = if directory.is_empty() {
        remote_key.to_string()
    } else {
        format!("{}/{}", directory.trim_matches('/'), remote_key)
    };

    let mut args = vec![
        "az".to_string(),
        "storage".to_string(),
        "blob".to_string(),
        "upload".to_string(),
        "--file".to_string(),
        local_path.to_string_lossy().into_owned(),
        "--container-name".to_string(),
        bucket.to_string(),
        "--name".to_string(),
        blob_name,
        "--overwrite".to_string(),
    ];

    if let Some(ref cache_control) = config.cache_control {
        args.push("--content-cache-control".to_string());
        args.push(cache_control.clone());
    }
    if let Some(ref content_disposition) = config.content_disposition {
        args.push("--content-disposition".to_string());
        args.push(content_disposition.clone());
    }

    args
}

// ---------------------------------------------------------------------------
// resolve_extra_files — expand glob patterns for extra_files
// ---------------------------------------------------------------------------

/// Resolve extra files from glob patterns.
pub fn resolve_extra_files(
    extra_files: &[anodize_core::config::ExtraFile],
) -> Result<Vec<(PathBuf, String)>> {
    let mut resolved = Vec::new();
    for ef in extra_files {
        let matches: Vec<_> = glob::glob(&ef.glob)
            .with_context(|| format!("invalid glob pattern: {}", ef.glob))?
            .filter_map(|entry| entry.ok())
            .collect();
        if matches.is_empty() {
            anyhow::bail!("blobs: extra_files glob '{}' matched no files", ef.glob);
        }
        for path in matches {
            let upload_name = if let Some(ref tmpl) = ef.name {
                tmpl.clone()
            } else {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string()
            };
            resolved.push((path, upload_name));
        }
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// BlobStage
// ---------------------------------------------------------------------------

pub struct BlobStage;

impl Stage for BlobStage {
    fn name(&self) -> &str {
        "blob"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("blob");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;

        // Collect crates that have blob config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.blobs.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        for krate in &crates {
            let blob_configs = krate.blobs.as_ref().unwrap();

            for blob_cfg in blob_configs {
                // Skip disabled configs
                if blob_cfg.disable.unwrap_or(false) {
                    log.status(&format!(
                        "skipping disabled blob config for crate {}",
                        krate.name
                    ));
                    continue;
                }

                // Validate required fields
                if blob_cfg.provider.is_empty() {
                    anyhow::bail!("blobs: provider is required for crate '{}'", krate.name);
                }
                if blob_cfg.bucket.is_empty() {
                    anyhow::bail!("blobs: bucket is required for crate '{}'", krate.name);
                }

                // Parse provider
                let provider = Provider::parse(&blob_cfg.provider)?;

                let config_label = blob_cfg.id.as_deref().unwrap_or(&blob_cfg.provider);

                // Render template fields
                let rendered_bucket = ctx.render_template(&blob_cfg.bucket).with_context(|| {
                    format!(
                        "blobs[{}]: render bucket template for crate {}",
                        config_label, krate.name
                    )
                })?;

                let directory_template = blob_cfg
                    .directory
                    .as_deref()
                    .unwrap_or("{{ ProjectName }}/{{ Tag }}");
                let rendered_directory =
                    ctx.render_template(directory_template).with_context(|| {
                        format!(
                            "blobs[{}]: render directory template for crate {}",
                            config_label, krate.name
                        )
                    })?;

                log.status(&format!(
                    "uploading to {} {}/{}",
                    blob_cfg.provider, rendered_bucket, rendered_directory
                ));

                // Collect artifacts to upload (unless extra_files_only is set)
                let mut upload_items: Vec<(PathBuf, String)> = Vec::new();

                if !blob_cfg.extra_files_only.unwrap_or(false) {
                    // Filter artifacts by ids if configured
                    let artifacts: Vec<&Artifact> = ctx
                        .artifacts
                        .all()
                        .iter()
                        .filter(|a| a.crate_name == krate.name)
                        .filter(|a| {
                            if let Some(ref filter_ids) = blob_cfg.ids {
                                if filter_ids.is_empty() {
                                    return true;
                                }
                                a.metadata
                                    .get("id")
                                    .map(|id| filter_ids.contains(id))
                                    .unwrap_or(false)
                                    || a.metadata
                                        .get("name")
                                        .map(|n| filter_ids.contains(n))
                                        .unwrap_or(false)
                            } else {
                                true
                            }
                        })
                        .collect();

                    for artifact in &artifacts {
                        let filename = artifact
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("artifact")
                            .to_string();
                        upload_items.push((artifact.path.clone(), filename));
                    }
                }

                // Resolve extra files
                if let Some(ref extra_files) = blob_cfg.extra_files {
                    let resolved = resolve_extra_files(extra_files)?;
                    upload_items.extend(resolved);
                }

                // Upload metadata files if include_meta is set
                if blob_cfg.include_meta.unwrap_or(false) {
                    let dist = &ctx.config.dist;
                    for meta_name in &["metadata.json", "artifacts.json"] {
                        let meta_path = dist.join(meta_name);
                        if meta_path.exists() {
                            upload_items.push((meta_path, (*meta_name).to_string()));
                        }
                    }
                }

                if upload_items.is_empty() {
                    log.warn(&format!(
                        "no files to upload for blob config on crate '{}'",
                        krate.name
                    ));
                    continue;
                }

                // Execute uploads
                for (local_path, remote_key) in &upload_items {
                    let cmd_args = build_upload_command(
                        provider,
                        blob_cfg,
                        local_path,
                        remote_key,
                        &rendered_bucket,
                        &rendered_directory,
                    );

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", cmd_args.join(" ")));
                        continue;
                    }

                    log.verbose(&format!("running: {}", cmd_args.join(" ")));

                    let mut cmd = Command::new(&cmd_args[0]);
                    cmd.args(&cmd_args[1..]);

                    // S3: enable path-style addressing for S3-compatible backends
                    if provider == Provider::S3 && blob_cfg.s3_force_path_style.unwrap_or(false) {
                        cmd.env("AWS_S3_ADDRESSING_STYLE", "path");
                    }

                    let output = cmd.output().with_context(|| {
                        format!(
                            "execute {} upload for {} to {}/{}",
                            provider.cli_binary(),
                            local_path.display(),
                            rendered_bucket,
                            rendered_directory,
                        )
                    })?;
                    log.check_output(
                        output,
                        &format!("{} upload: {}", blob_cfg.provider, remote_key),
                    )?;
                }

                log.status(&format!(
                    "uploaded {} file(s) to {} {}/{}",
                    upload_items.len(),
                    blob_cfg.provider,
                    rendered_bucket,
                    rendered_directory,
                ));
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
    use anodize_core::config::BlobConfig;
    use std::path::Path;

    fn default_blob_config() -> BlobConfig {
        BlobConfig {
            provider: "s3".to_string(),
            bucket: "my-bucket".to_string(),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Provider parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_provider_s3() {
        assert_eq!(Provider::parse("s3").unwrap(), Provider::S3);
    }

    #[test]
    fn test_provider_gcs() {
        assert_eq!(Provider::parse("gcs").unwrap(), Provider::Gcs);
        assert_eq!(Provider::parse("gs").unwrap(), Provider::Gcs);
    }

    #[test]
    fn test_provider_azblob() {
        assert_eq!(Provider::parse("azblob").unwrap(), Provider::AzBlob);
        assert_eq!(Provider::parse("azure").unwrap(), Provider::AzBlob);
    }

    #[test]
    fn test_provider_invalid() {
        let result = Provider::parse("dropbox");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown provider"), "got: {}", msg);
        assert!(msg.contains("dropbox"), "got: {}", msg);
    }

    #[test]
    fn test_provider_cli_binary() {
        assert_eq!(Provider::S3.cli_binary(), "aws");
        assert_eq!(Provider::Gcs.cli_binary(), "gsutil");
        assert_eq!(Provider::AzBlob.cli_binary(), "az");
    }

    // -----------------------------------------------------------------------
    // S3 command construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_s3_basic_command() {
        let cfg = default_blob_config();
        let args = build_upload_command(
            Provider::S3,
            &cfg,
            Path::new("/tmp/artifact.tar.gz"),
            "artifact.tar.gz",
            "my-bucket",
            "releases/v1.0.0",
        );
        assert_eq!(args[0], "aws");
        assert_eq!(args[1], "s3");
        assert_eq!(args[2], "cp");
        assert_eq!(args[3], "/tmp/artifact.tar.gz");
        assert_eq!(args[4], "s3://my-bucket/releases/v1.0.0/artifact.tar.gz");
    }

    #[test]
    fn test_s3_empty_directory() {
        let cfg = default_blob_config();
        let args = build_upload_command(
            Provider::S3,
            &cfg,
            Path::new("/tmp/file.txt"),
            "file.txt",
            "my-bucket",
            "",
        );
        assert_eq!(args[4], "s3://my-bucket/file.txt");
    }

    #[test]
    fn test_s3_with_region() {
        let cfg = BlobConfig {
            region: Some("eu-central-1".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(
            Provider::S3,
            &cfg,
            Path::new("/tmp/f.tar.gz"),
            "f.tar.gz",
            "b",
            "d",
        );
        assert!(args.contains(&"--region".to_string()));
        assert!(args.contains(&"eu-central-1".to_string()));
    }

    #[test]
    fn test_s3_with_endpoint() {
        let cfg = BlobConfig {
            endpoint: Some("http://localhost:9000".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--endpoint-url".to_string()));
        assert!(args.contains(&"http://localhost:9000".to_string()));
    }

    #[test]
    fn test_s3_with_acl() {
        let cfg = BlobConfig {
            acl: Some("public-read".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--acl".to_string()));
        assert!(args.contains(&"public-read".to_string()));
    }

    #[test]
    fn test_s3_with_cache_control() {
        let cfg = BlobConfig {
            cache_control: Some("max-age=86400".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--cache-control".to_string()));
        assert!(args.contains(&"max-age=86400".to_string()));
    }

    #[test]
    fn test_s3_with_content_disposition() {
        let cfg = BlobConfig {
            content_disposition: Some("attachment;filename=release.tar.gz".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--content-disposition".to_string()));
        assert!(args.contains(&"attachment;filename=release.tar.gz".to_string()));
    }

    #[test]
    fn test_s3_with_kms_key() {
        let cfg = BlobConfig {
            kms_key: Some("arn:aws:kms:us-east-1:123:key/abc".to_string()),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--sse".to_string()));
        assert!(args.contains(&"aws:kms".to_string()));
        assert!(args.contains(&"--sse-kms-key-id".to_string()));
        assert!(args.contains(&"arn:aws:kms:us-east-1:123:key/abc".to_string()));
    }

    #[test]
    fn test_s3_disable_ssl() {
        let cfg = BlobConfig {
            disable_ssl: Some(true),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(args.contains(&"--no-verify-ssl".to_string()));
    }

    #[test]
    fn test_s3_disable_ssl_false() {
        let cfg = BlobConfig {
            disable_ssl: Some(false),
            ..default_blob_config()
        };
        let args = build_upload_command(Provider::S3, &cfg, Path::new("/tmp/f"), "f", "b", "d");
        assert!(!args.contains(&"--no-verify-ssl".to_string()));
    }

    #[test]
    fn test_s3_all_options() {
        let cfg = BlobConfig {
            provider: "s3".to_string(),
            bucket: "my-bucket".to_string(),
            region: Some("us-west-2".to_string()),
            endpoint: Some("http://minio:9000".to_string()),
            acl: Some("private".to_string()),
            cache_control: Some("no-cache".to_string()),
            content_disposition: Some("inline".to_string()),
            kms_key: Some("key123".to_string()),
            disable_ssl: Some(true),
            ..Default::default()
        };
        let args = build_upload_command(
            Provider::S3,
            &cfg,
            Path::new("/tmp/f"),
            "f",
            "my-bucket",
            "dir",
        );
        assert!(args.contains(&"--region".to_string()));
        assert!(args.contains(&"--endpoint-url".to_string()));
        assert!(args.contains(&"--acl".to_string()));
        assert!(args.contains(&"--cache-control".to_string()));
        assert!(args.contains(&"--content-disposition".to_string()));
        assert!(args.contains(&"--sse".to_string()));
        assert!(args.contains(&"--no-verify-ssl".to_string()));
    }

    // -----------------------------------------------------------------------
    // GCS command construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_gcs_basic_command() {
        let cfg = BlobConfig {
            provider: "gcs".to_string(),
            bucket: "my-gcs-bucket".to_string(),
            ..Default::default()
        };
        let args = build_upload_command(
            Provider::Gcs,
            &cfg,
            Path::new("/tmp/artifact.tar.gz"),
            "artifact.tar.gz",
            "my-gcs-bucket",
            "releases/v1.0.0",
        );
        assert_eq!(args[0], "gsutil");
        assert_eq!(args[1], "cp");
        assert_eq!(args[2], "/tmp/artifact.tar.gz");
        assert_eq!(
            args[3],
            "gs://my-gcs-bucket/releases/v1.0.0/artifact.tar.gz"
        );
    }

    #[test]
    fn test_gcs_empty_directory() {
        let cfg = BlobConfig {
            provider: "gcs".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        let args = build_upload_command(Provider::Gcs, &cfg, Path::new("/tmp/f"), "f", "b", "");
        assert_eq!(args[3], "gs://b/f");
    }

    // -----------------------------------------------------------------------
    // Azure Blob command construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_azblob_basic_command() {
        let cfg = BlobConfig {
            provider: "azblob".to_string(),
            bucket: "mycontainer".to_string(),
            ..Default::default()
        };
        let args = build_upload_command(
            Provider::AzBlob,
            &cfg,
            Path::new("/tmp/artifact.tar.gz"),
            "artifact.tar.gz",
            "mycontainer",
            "releases/v1.0.0",
        );
        assert_eq!(args[0], "az");
        assert_eq!(args[1], "storage");
        assert_eq!(args[2], "blob");
        assert_eq!(args[3], "upload");
        assert_eq!(args[4], "--file");
        assert_eq!(args[5], "/tmp/artifact.tar.gz");
        assert_eq!(args[6], "--container-name");
        assert_eq!(args[7], "mycontainer");
        assert_eq!(args[8], "--name");
        assert_eq!(args[9], "releases/v1.0.0/artifact.tar.gz");
        assert_eq!(args[10], "--overwrite");
    }

    #[test]
    fn test_azblob_empty_directory() {
        let cfg = BlobConfig {
            provider: "azblob".to_string(),
            bucket: "c".to_string(),
            ..Default::default()
        };
        let args = build_upload_command(Provider::AzBlob, &cfg, Path::new("/tmp/f"), "f", "c", "");
        assert_eq!(args[9], "f");
    }

    // -----------------------------------------------------------------------
    // Directory trimming
    // -----------------------------------------------------------------------

    #[test]
    fn test_directory_trim_slashes() {
        let cfg = default_blob_config();
        let args = build_upload_command(
            Provider::S3,
            &cfg,
            Path::new("/tmp/f"),
            "f",
            "b",
            "/leading/trailing/",
        );
        assert_eq!(args[4], "s3://b/leading/trailing/f");
    }

    // -----------------------------------------------------------------------
    // Extra file resolution
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_extra_files_basic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("readme.txt");
        std::fs::write(&file, "hello").unwrap();

        let extras = vec![anodize_core::config::ExtraFile {
            glob: file.to_string_lossy().into_owned(),
            name: None,
        }];

        let resolved = resolve_extra_files(&extras).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, file);
        assert_eq!(resolved[0].1, "readme.txt");
    }

    #[test]
    fn test_resolve_extra_files_with_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("LICENSE");
        std::fs::write(&file, "MIT").unwrap();

        let extras = vec![anodize_core::config::ExtraFile {
            glob: file.to_string_lossy().into_owned(),
            name: Some("LICENSE.txt".to_string()),
        }];

        let resolved = resolve_extra_files(&extras).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1, "LICENSE.txt");
    }

    #[test]
    fn test_resolve_extra_files_glob_pattern() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "b").unwrap();
        std::fs::write(tmp.path().join("c.log"), "c").unwrap();

        let pattern = format!("{}/*.txt", tmp.path().display());
        let extras = vec![anodize_core::config::ExtraFile {
            glob: pattern,
            name: None,
        }];

        let resolved = resolve_extra_files(&extras).unwrap();
        assert_eq!(resolved.len(), 2);
        let names: Vec<&str> = resolved.iter().map(|(_, n)| n.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[test]
    fn test_resolve_extra_files_no_match() {
        let extras = vec![anodize_core::config::ExtraFile {
            glob: "/nonexistent/path/to/*.xyz".to_string(),
            name: None,
        }];

        let result = resolve_extra_files(&extras);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("matched no files"), "got: {}", msg);
    }

    // -----------------------------------------------------------------------
    // Config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_blob_config_parses_s3() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: my-releases
    directory: "{{ ProjectName }}/{{ Tag }}"
    region: us-east-1
    acl: public-read
    cache_control: "max-age=86400"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].provider, "s3");
        assert_eq!(blobs[0].bucket, "my-releases");
        assert_eq!(blobs[0].region.as_deref(), Some("us-east-1"));
        assert_eq!(blobs[0].acl.as_deref(), Some("public-read"));
    }

    #[test]
    fn test_blob_config_parses_gcs() {
        let yaml = r#"
blobs:
  - provider: gcs
    bucket: my-gcs-bucket
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs[0].provider, "gcs");
        assert_eq!(blobs[0].bucket, "my-gcs-bucket");
    }

    #[test]
    fn test_blob_config_parses_azblob() {
        let yaml = r#"
blobs:
  - provider: azblob
    bucket: mycontainer
    directory: releases/{{ Version }}
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs[0].provider, "azblob");
        assert_eq!(blobs[0].bucket, "mycontainer");
        assert_eq!(
            blobs[0].directory.as_deref(),
            Some("releases/{{ Version }}")
        );
    }

    #[test]
    fn test_blob_config_multiple_providers() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: s3-bucket
    region: us-west-2
  - provider: gcs
    bucket: gcs-bucket
  - provider: azblob
    bucket: azure-container
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs.len(), 3);
        assert_eq!(blobs[0].provider, "s3");
        assert_eq!(blobs[1].provider, "gcs");
        assert_eq!(blobs[2].provider, "azblob");
    }

    #[test]
    fn test_blob_config_all_fields() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: my-bucket
    directory: "releases/v1"
    region: eu-central-1
    endpoint: "http://minio:9000"
    disable_ssl: true
    s3_force_path_style: true
    acl: private
    cache_control: "no-cache"
    content_disposition: "inline"
    kms_key: "key123"
    ids:
      - build-linux
    disable: false
    include_meta: true
    extra_files:
      - glob: "LICENSE*"
    extra_files_only: false
    id: my-blob-config
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        let b = &blobs[0];
        assert_eq!(b.provider, "s3");
        assert_eq!(b.bucket, "my-bucket");
        assert_eq!(b.directory.as_deref(), Some("releases/v1"));
        assert_eq!(b.region.as_deref(), Some("eu-central-1"));
        assert_eq!(b.endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(b.disable_ssl, Some(true));
        assert_eq!(b.s3_force_path_style, Some(true));
        assert_eq!(b.acl.as_deref(), Some("private"));
        assert_eq!(b.cache_control.as_deref(), Some("no-cache"));
        assert_eq!(b.content_disposition.as_deref(), Some("inline"));
        assert_eq!(b.kms_key.as_deref(), Some("key123"));
        assert_eq!(b.ids.as_ref().unwrap(), &["build-linux"]);
        assert_eq!(b.disable, Some(false));
        assert_eq!(b.include_meta, Some(true));
        assert!(b.extra_files.is_some());
        assert_eq!(b.extra_files_only, Some(false));
        assert_eq!(b.id.as_deref(), Some("my-blob-config"));
    }

    #[test]
    fn test_blob_config_defaults() {
        let cfg = BlobConfig::default();
        assert!(cfg.provider.is_empty());
        assert!(cfg.bucket.is_empty());
        assert!(cfg.directory.is_none());
        assert!(cfg.region.is_none());
        assert!(cfg.endpoint.is_none());
        assert!(cfg.disable_ssl.is_none());
        assert!(cfg.s3_force_path_style.is_none());
        assert!(cfg.acl.is_none());
        assert!(cfg.cache_control.is_none());
        assert!(cfg.content_disposition.is_none());
        assert!(cfg.kms_key.is_none());
        assert!(cfg.ids.is_none());
        assert!(cfg.disable.is_none());
        assert!(cfg.include_meta.is_none());
        assert!(cfg.extra_files.is_none());
        assert!(cfg.extra_files_only.is_none());
        assert!(cfg.id.is_none());
    }

    #[test]
    fn test_blob_config_extra_files_with_name() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    extra_files:
      - glob: "./LICENSE"
        name: "LICENSE.txt"
      - glob: "./README.md"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        let extras = blobs[0].extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].glob, "./LICENSE");
        assert_eq!(extras[0].name.as_deref(), Some("LICENSE.txt"));
        assert_eq!(extras[1].glob, "./README.md");
        assert!(extras[1].name.is_none());
    }

    #[test]
    fn test_partial_config_parses() {
        let yaml = r#"
partial:
  by: target
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let partial = config.partial.unwrap();
        assert_eq!(partial.by.as_deref(), Some("target"));
    }

    #[test]
    fn test_partial_config_defaults() {
        let cfg = anodize_core::config::PartialConfig::default();
        assert!(cfg.by.is_none());
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests (dry-run)
    // -----------------------------------------------------------------------

    #[test]
    fn test_blob_stage_skips_when_no_config() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_stage_skips_disabled_config() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: "b".to_string(),
                    disable: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_stage_invalid_provider() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "dropbox".to_string(),
                    bucket: "b".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown provider"), "got: {}", msg);
    }

    #[test]
    fn test_blob_stage_dry_run_logs_commands() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;

        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: "my-bucket".to_string(),
                    directory: Some("releases".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        // Add a fake artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/test.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mycrate".to_string(),
            metadata: HashMap::new(),
        });

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok(), "dry-run should succeed: {:?}", result.err());
    }

    #[test]
    fn test_blob_stage_extra_files_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let extra = tmp.path().join("extra.txt");
        std::fs::write(&extra, "data").unwrap();

        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: "b".to_string(),
                    directory: Some("d".to_string()),
                    extra_files: Some(vec![anodize_core::config::ExtraFile {
                        glob: extra.to_string_lossy().into_owned(),
                        name: None,
                    }]),
                    extra_files_only: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        // Add an artifact that should NOT be uploaded
        ctx.artifacts.add(Artifact {
            kind: anodize_core::artifact::ArtifactKind::Archive,
            path: PathBuf::from("/tmp/should-not-upload.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: std::collections::HashMap::new(),
        });

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_stage_ids_filter() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;

        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "gcs".to_string(),
                    bucket: "b".to_string(),
                    directory: Some("d".to_string()),
                    ids: Some(vec!["linux-build".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        // Add two artifacts, only one should match the ids filter
        let mut meta_linux = HashMap::new();
        meta_linux.insert("id".to_string(), "linux-build".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/linux.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: meta_linux,
        });

        let mut meta_windows = HashMap::new();
        meta_windows.insert("id".to_string(), "windows-build".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("/tmp/windows.zip"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: meta_windows,
        });

        let stage = BlobStage;
        // In dry-run mode this will succeed; the filter logic only allows linux-build
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_blob_stage_include_meta() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("metadata.json"), r#"{"version":"1"}"#).unwrap();
        std::fs::write(dist.join("artifacts.json"), r#"{"artifacts":[]}"#).unwrap();

        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            dist: dist.clone(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: "b".to_string(),
                    directory: Some("d".to_string()),
                    include_meta: Some(true),
                    extra_files_only: Some(true), // skip artifacts, only meta
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions {
            dry_run: true,
            ..Default::default()
        };
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "include_meta with existing metadata files should succeed in dry-run: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_blob_stage_empty_provider_error() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: String::new(),
                    bucket: "b".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("provider is required"), "got: {}", msg);
    }

    #[test]
    fn test_blob_stage_empty_bucket_error() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: String::new(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = anodize_core::context::ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("bucket is required"), "got: {}", msg);
    }
}
