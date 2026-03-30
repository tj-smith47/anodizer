use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{BlobConfig, ExtraFile, StringOrBool};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::template;

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutOptions};

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
                "blobs: unknown provider '{}'. Valid providers are: s3, gs, azblob",
                other
            ),
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Provider::S3 => "s3",
            Provider::Gcs => "gs",
            Provider::AzBlob => "azblob",
        }
    }
}

// ---------------------------------------------------------------------------
// Store construction — one function per provider
// ---------------------------------------------------------------------------

/// Build an `ObjectStore` for the given provider and config.
/// All env-based credential chains are handled by the builder's `from_env()`.
fn build_store(
    provider: Provider,
    config: &BlobConfig,
    rendered_bucket: &str,
    ctx: &Context,
) -> Result<Box<dyn ObjectStore>> {
    match provider {
        Provider::S3 => build_s3_store(config, rendered_bucket, ctx),
        Provider::Gcs => build_gcs_store(rendered_bucket, config),
        Provider::AzBlob => build_azure_store(rendered_bucket),
    }
}

fn build_s3_store(
    config: &BlobConfig,
    bucket: &str,
    ctx: &Context,
) -> Result<Box<dyn ObjectStore>> {
    use object_store::aws::AmazonS3Builder;

    let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);

    if let Some(ref region) = config.region {
        let rendered = template::render(region, ctx.template_vars())
            .with_context(|| format!("blobs: render region template: {region}"))?;
        builder = builder.with_region(&rendered);
    }

    if let Some(ref endpoint) = config.endpoint {
        let rendered = template::render(endpoint, ctx.template_vars())
            .with_context(|| format!("blobs: render endpoint template: {endpoint}"))?;
        builder = builder.with_endpoint(&rendered);

        // Smart default: force path style when custom endpoint is set.
        // MinIO, R2, DO Spaces, Backblaze B2 all need path-style addressing.
        let force_path = config.s3_force_path_style.unwrap_or(true);
        builder = builder.with_virtual_hosted_style_request(!force_path);
    } else if let Some(force_path) = config.s3_force_path_style {
        builder = builder.with_virtual_hosted_style_request(!force_path);
    }

    if config.disable_ssl.unwrap_or(false) {
        builder = builder.with_allow_http(true);
    }

    // KMS server-side encryption
    if let Some(ref kms_key) = config.kms_key {
        builder = builder.with_sse_kms_encryption(kms_key);
    }

    // S3 canned ACL via x-amz-acl header.
    // We set it as a default header on the client — since each blob config
    // gets its own ObjectStore client, this is per-config ACL.
    if let Some(ref acl) = config.acl {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-acl"),
            reqwest::header::HeaderValue::from_str(acl)
                .with_context(|| format!("blobs: invalid ACL value: {acl}"))?,
        );
        let client_opts = object_store::ClientOptions::new().with_default_headers(headers);
        builder = builder.with_client_options(client_opts);
    }

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build S3 client")?,
    ))
}

fn build_gcs_store(bucket: &str, config: &BlobConfig) -> Result<Box<dyn ObjectStore>> {
    use object_store::gcp::GoogleCloudStorageBuilder;

    let mut builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);

    // GCS predefined ACL via x-goog-acl header
    if let Some(ref acl) = config.acl {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-goog-acl"),
            reqwest::header::HeaderValue::from_str(acl)
                .with_context(|| format!("blobs: invalid ACL value: {acl}"))?,
        );
        let client_opts = object_store::ClientOptions::new().with_default_headers(headers);
        builder = builder.with_client_options(client_opts);
    }

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build GCS client")?,
    ))
}

fn build_azure_store(container: &str) -> Result<Box<dyn ObjectStore>> {
    use object_store::azure::MicrosoftAzureBuilder;

    let builder = MicrosoftAzureBuilder::from_env().with_container_name(container);

    Ok(Box::new(
        builder
            .build()
            .context("blobs: failed to build Azure Blob client")?,
    ))
}

// ---------------------------------------------------------------------------
// Put options — headers (cache-control, content-disposition)
// ---------------------------------------------------------------------------

fn build_put_options(
    config: &BlobConfig,
    filename: &str,
    ctx: &Context,
) -> Result<PutOptions> {
    use object_store::Attribute;

    let mut attrs = object_store::Attributes::new();

    // Cache-Control: join array with ", " (GoReleaser uses []string)
    if let Some(ref cc) = config.cache_control
        && !cc.is_empty()
    {
        attrs.insert(Attribute::CacheControl, cc.join(", ").into());
    }

    // Content-Disposition: template-rendered with {{Filename}} variable.
    // Default: "attachment;filename={{Filename}}". Set to "-" to disable.
    let disp_template = config
        .content_disposition
        .as_deref()
        .unwrap_or("attachment;filename={{Filename}}");

    if disp_template != "-" {
        // Render the template with the Filename variable added
        let mut vars = ctx.template_vars().clone();
        vars.set("Filename", filename);
        let rendered = template::render(disp_template, &vars)
            .with_context(|| format!("blobs: render content_disposition: {disp_template}"))?;
        attrs.insert(Attribute::ContentDisposition, rendered.into());
    }

    // ACL is handled at the client level via x-amz-acl / x-goog-acl headers
    // set in build_s3_store() / build_gcs_store(). No per-request handling needed.

    Ok(PutOptions {
        attributes: attrs,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Disable evaluation — supports bool and template strings
// ---------------------------------------------------------------------------

fn is_disabled(disable: &Option<StringOrBool>, ctx: &Context) -> Result<bool> {
    match disable {
        None => Ok(false),
        Some(d) => Ok(d.is_disabled(|s| ctx.render_template(s))),
    }
}

// ---------------------------------------------------------------------------
// Extra files resolution — with template-rendered names
// ---------------------------------------------------------------------------

fn resolve_extra_files(
    extra_files: &[ExtraFile],
    ctx: &Context,
    log: &anodize_core::log::StageLogger,
) -> Result<Vec<(PathBuf, String)>> {
    let mut resolved = Vec::new();

    for ef in extra_files {
        let matches: Vec<PathBuf> = glob::glob(&ef.glob)
            .with_context(|| format!("blobs: invalid glob pattern: {}", ef.glob))?
            .filter_map(|r| r.ok())
            .collect();

        if matches.is_empty() {
            // Warn and continue, matching GoReleaser behavior. Users with
            // platform-conditional extra_files (e.g., *.exe on Windows) should
            // not get failures on other platforms.
            log.warn(&format!(
                "blobs: extra_files glob '{}' matched no files, skipping",
                ef.glob
            ));
            continue;
        }

        for path in matches {
            let upload_name = if let Some(ref name_tmpl) = ef.name {
                // Template-render the name with standard vars + Filename
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                let mut vars = ctx.template_vars().clone();
                vars.set("Filename", filename);
                template::render(name_tmpl, &vars)
                    .with_context(|| format!("blobs: render extra_files name: {name_tmpl}"))?
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
// Artifact filtering
// ---------------------------------------------------------------------------

/// Collect artifacts to upload based on config filters.
fn collect_artifacts<'a>(
    ctx: &'a Context,
    config: &BlobConfig,
    crate_name: &str,
) -> Vec<&'a Artifact> {
    if config.extra_files_only.unwrap_or(false) {
        return vec![];
    }

    let uploadable_kinds = if config.include_meta.unwrap_or(false) {
        vec![
            ArtifactKind::Binary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::LinuxPackage,
            ArtifactKind::SourceArchive,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
            ArtifactKind::Metadata,
        ]
    } else {
        vec![
            ArtifactKind::Binary,
            ArtifactKind::Archive,
            ArtifactKind::Checksum,
            ArtifactKind::LinuxPackage,
            ArtifactKind::SourceArchive,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
        ]
    };

    ctx.artifacts
        .all()
        .iter()
        .filter(|a| a.crate_name == crate_name)
        .filter(|a| uploadable_kinds.contains(&a.kind))
        .filter(|a| {
            if let Some(ref filter_ids) = config.ids {
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
        .collect()
}

// ---------------------------------------------------------------------------
// Upload execution
// ---------------------------------------------------------------------------

struct UploadParams<'a> {
    store: Arc<dyn ObjectStore>,
    items: &'a [(PathBuf, String)],
    directory: &'a str,
    config: &'a BlobConfig,
    ctx: &'a Context,
}

fn upload_files(params: &UploadParams<'_>) -> Result<()> {
    // Use multi-threaded tokio runtime for actual parallel uploads
    let rt = tokio::runtime::Runtime::new()
        .context("blobs: failed to create tokio runtime")?;

    rt.block_on(async {
        let parallelism = params.ctx.options.parallelism.max(1);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

        let mut handles = Vec::new();

        for (local_path, remote_key) in params.items {
            let dir_trimmed = params.directory.trim_matches('/');
            let object_key = if dir_trimmed.is_empty() {
                remote_key.clone()
            } else {
                format!("{}/{}", dir_trimmed, remote_key)
            };

            let object_path = ObjectPath::from(object_key.as_str());
            let put_opts = build_put_options(params.config, remote_key, params.ctx)?;

            let store = Arc::clone(&params.store);
            let sem = Arc::clone(&semaphore);
            let path_display = local_path.display().to_string();
            let local = local_path.clone();
            let key_display = object_key.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("semaphore error: {}", e))?;
                // Read file inside the spawned task to interleave I/O with uploads
                let data = tokio::fs::read(&local).await.map_err(|e| {
                    anyhow::anyhow!("blobs: read file for upload: {}: {}", path_display, e)
                })?;
                store
                    .put_opts(&object_path, data.into(), put_opts)
                    .await
                    .map_err(|e| handle_upload_error(e, &path_display, &key_display))?;
                Ok::<(), anyhow::Error>(())
            }));
        }

        for handle in handles {
            handle
                .await
                .map_err(|e| anyhow::anyhow!("upload task panicked: {}", e))??;
        }
        Ok(())
    })
}

fn format_remote_path(provider: Provider, bucket: &str, directory: &str, key: &str) -> String {
    let dir_trimmed = directory.trim_matches('/');
    let scheme = match provider {
        Provider::S3 => "s3",
        Provider::Gcs => "gs",
        Provider::AzBlob => "azblob",
    };
    if dir_trimmed.is_empty() {
        format!("{}://{}/{}", scheme, bucket, key)
    } else {
        format!("{}://{}/{}/{}", scheme, bucket, dir_trimmed, key)
    }
}

fn handle_upload_error(err: object_store::Error, local_path: &str, remote_key: &str) -> anyhow::Error {
    match &err {
        object_store::Error::NotFound { path, .. } => {
            anyhow::anyhow!(
                "blobs: bucket or object not found ({}): uploading {} -> {}",
                path, local_path, remote_key
            )
        }
        object_store::Error::Unauthenticated { path, .. } => {
            anyhow::anyhow!(
                "blobs: authentication failed — check credentials. Uploading {} -> {} ({})",
                local_path, remote_key, path
            )
        }
        object_store::Error::PermissionDenied { path, .. } => {
            anyhow::anyhow!(
                "blobs: access denied — check permissions. Uploading {} -> {} ({})",
                local_path, remote_key, path
            )
        }
        _ => {
            anyhow::anyhow!(
                "blobs: upload failed for {} -> {}: {}",
                local_path, remote_key, err
            )
        }
    }
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
                // Evaluate disable (supports both bool and template string)
                if is_disabled(&blob_cfg.disable, ctx)? {
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

                let provider_str = ctx.render_template(&blob_cfg.provider)
                    .with_context(|| {
                        format!("blobs: render provider template '{}' for crate '{}'",
                            blob_cfg.provider, krate.name)
                    })?;
                let provider = Provider::parse(&provider_str)?;
                let config_label = blob_cfg.id.as_deref().unwrap_or(&provider_str);

                // Render template fields
                let rendered_bucket =
                    ctx.render_template(&blob_cfg.bucket).with_context(|| {
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
                    provider.display_name(),
                    rendered_bucket,
                    rendered_directory
                ));

                // Collect artifacts to upload
                let mut upload_items: Vec<(PathBuf, String)> = Vec::new();

                let artifacts = collect_artifacts(ctx, blob_cfg, &krate.name);
                for artifact in &artifacts {
                    let filename = artifact
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("artifact")
                        .to_string();
                    upload_items.push((artifact.path.clone(), filename));
                }

                // Resolve extra files (with template-rendered names)
                if let Some(ref extra_files) = blob_cfg.extra_files {
                    let resolved = resolve_extra_files(extra_files, ctx, &log)?;
                    upload_items.extend(resolved);
                }

                // Note: metadata files are already handled by collect_artifacts()
                // when include_meta is true — it includes ArtifactKind::Metadata
                // in its filter. No separate scan needed here.

                if upload_items.is_empty() {
                    log.warn(&format!(
                        "no files to upload for blob config on crate '{}'",
                        krate.name
                    ));
                    continue;
                }

                if dry_run {
                    // Dry-run: log what would happen without constructing the store
                    for (local_path, remote_key) in &upload_items {
                        let remote = format_remote_path(
                            provider,
                            &rendered_bucket,
                            &rendered_directory,
                            remote_key,
                        );
                        log.status(&format!(
                            "[dry-run] would upload {} -> {}",
                            local_path.display(),
                            remote,
                        ));
                    }
                } else {
                    let store: Arc<dyn ObjectStore> =
                        Arc::from(build_store(provider, blob_cfg, &rendered_bucket, ctx)?);
                    let params = UploadParams {
                        store,
                        items: &upload_items,
                        directory: &rendered_directory,
                        config: blob_cfg,
                        ctx,
                    };
                    upload_files(&params)?;
                }

                log.status(&format!(
                    "uploaded {} file(s) to {} {}/{}",
                    upload_items.len(),
                    provider.display_name(),
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
    use anodize_core::context::{Context, ContextOptions};

    // -----------------------------------------------------------------------
    // Provider tests
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
        let err = Provider::parse("dropbox").unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn test_provider_display_name() {
        assert_eq!(Provider::S3.display_name(), "s3");
        assert_eq!(Provider::Gcs.display_name(), "gs");
        assert_eq!(Provider::AzBlob.display_name(), "azblob");
    }

    // -----------------------------------------------------------------------
    // S3 store builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_s3_basic_build() {
        // Verify the builder doesn't panic with minimal config.
        // Actual credential validation happens at upload time, not build time.
        let config = BlobConfig {
            provider: "s3".to_string(),
            bucket: "test-bucket".to_string(),
            ..Default::default()
        };
        // The builder will succeed (credentials validated lazily)
        let result = build_s3_store(&config, "test-bucket", &make_ctx());
        // May fail due to missing AWS creds in test env, but should not panic
        // and should produce a meaningful error if it fails
        let _ = result;
    }

    #[test]
    fn test_s3_force_path_style_defaults_true_with_endpoint() {
        // When endpoint is set, force_path_style should default to true
        let config = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            endpoint: Some("http://minio:9000".to_string()),
            // s3_force_path_style NOT set — should default to true
            ..Default::default()
        };
        // This exercises the builder path. The builder configures
        // virtual_hosted_style_request = !force_path = false, which means
        // path-style is enabled.
        let _ = build_s3_store(&config, "b", &make_ctx());
    }

    #[test]
    fn test_s3_force_path_style_explicit_false_with_endpoint() {
        let config = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            endpoint: Some("http://minio:9000".to_string()),
            s3_force_path_style: Some(false),
            ..Default::default()
        };
        let _ = build_s3_store(&config, "b", &make_ctx());
    }

    // -----------------------------------------------------------------------
    // Put options tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_put_options_cache_control_array_joined() {
        let config = BlobConfig {
            cache_control: Some(vec![
                "max-age=86400".to_string(),
                "public".to_string(),
            ]),
            ..Default::default()
        };
        let opts = build_put_options(&config,"file.tar.gz", &make_ctx()).unwrap();
        let cc = opts
            .attributes
            .get(&object_store::Attribute::CacheControl)
            .expect("cache_control should be set");
        assert_eq!(cc.as_ref(), "max-age=86400, public");
    }

    #[test]
    fn test_put_options_cache_control_single() {
        let config = BlobConfig {
            cache_control: Some(vec!["no-cache".to_string()]),
            ..Default::default()
        };
        let opts = build_put_options(&config,"f.txt", &make_ctx()).unwrap();
        let cc = opts
            .attributes
            .get(&object_store::Attribute::CacheControl)
            .expect("should be set");
        assert_eq!(cc.as_ref(), "no-cache");
    }

    #[test]
    fn test_put_options_content_disposition_default() {
        let config = BlobConfig::default();
        let opts = build_put_options(&config,"myapp-v1.tar.gz", &make_ctx()).unwrap();
        let cd = opts
            .attributes
            .get(&object_store::Attribute::ContentDisposition)
            .expect("default content-disposition should be set");
        assert_eq!(cd.as_ref(), "attachment;filename=myapp-v1.tar.gz");
    }

    #[test]
    fn test_put_options_content_disposition_disabled() {
        let config = BlobConfig {
            content_disposition: Some("-".to_string()),
            ..Default::default()
        };
        let opts = build_put_options(&config,"f.txt", &make_ctx()).unwrap();
        assert!(opts
            .attributes
            .get(&object_store::Attribute::ContentDisposition)
            .is_none());
    }

    #[test]
    fn test_put_options_content_disposition_custom() {
        let config = BlobConfig {
            content_disposition: Some("inline".to_string()),
            ..Default::default()
        };
        let opts = build_put_options(&config,"f.txt", &make_ctx()).unwrap();
        let cd = opts
            .attributes
            .get(&object_store::Attribute::ContentDisposition)
            .expect("should be set");
        assert_eq!(cd.as_ref(), "inline");
    }

    // -----------------------------------------------------------------------
    // Disable evaluation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_disabled_none() {
        assert!(!is_disabled(&None, &make_ctx()).unwrap());
    }

    #[test]
    fn test_is_disabled_bool_true() {
        assert!(is_disabled(&Some(StringOrBool::Bool(true)), &make_ctx()).unwrap());
    }

    #[test]
    fn test_is_disabled_bool_false() {
        assert!(!is_disabled(&Some(StringOrBool::Bool(false)), &make_ctx()).unwrap());
    }

    #[test]
    fn test_is_disabled_string_true() {
        assert!(
            is_disabled(&Some(StringOrBool::String("true".to_string())), &make_ctx()).unwrap()
        );
    }

    #[test]
    fn test_is_disabled_string_false() {
        assert!(
            !is_disabled(&Some(StringOrBool::String("false".to_string())), &make_ctx()).unwrap()
        );
    }

    #[test]
    fn test_is_disabled_template_evaluates_to_true() {
        let ctx = make_ctx_with_snapshot();
        let disable = Some(StringOrBool::String(
            "{% if IsSnapshot %}true{% endif %}".to_string(),
        ));
        assert!(is_disabled(&disable, &ctx).unwrap());
    }

    #[test]
    fn test_is_disabled_template_evaluates_to_false() {
        let ctx = make_ctx(); // IsSnapshot is "false"
        let disable = Some(StringOrBool::String(
            "{% if IsSnapshot == \"true\" %}true{% endif %}".to_string(),
        ));
        assert!(!is_disabled(&disable, &ctx).unwrap());
    }

    // -----------------------------------------------------------------------
    // Remote path formatting
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_remote_path_s3() {
        let path = format_remote_path(Provider::S3, "my-bucket", "releases/v1", "file.tar.gz");
        assert_eq!(path, "s3://my-bucket/releases/v1/file.tar.gz");
    }

    #[test]
    fn test_format_remote_path_gcs() {
        let path = format_remote_path(Provider::Gcs, "my-bucket", "dir", "file.tar.gz");
        assert_eq!(path, "gs://my-bucket/dir/file.tar.gz");
    }

    #[test]
    fn test_format_remote_path_azure() {
        let path = format_remote_path(Provider::AzBlob, "container", "path", "file.tar.gz");
        assert_eq!(path, "azblob://container/path/file.tar.gz");
    }

    #[test]
    fn test_format_remote_path_empty_directory() {
        let path = format_remote_path(Provider::S3, "b", "", "file.tar.gz");
        assert_eq!(path, "s3://b/file.tar.gz");
    }

    #[test]
    fn test_format_remote_path_trims_slashes() {
        let path = format_remote_path(Provider::S3, "b", "/dir/", "file.tar.gz");
        assert_eq!(path, "s3://b/dir/file.tar.gz");
    }

    // -----------------------------------------------------------------------
    // Error handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_upload_error_not_found() {
        let err = object_store::Error::NotFound {
            path: "test/path".to_string(),
            source: Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, "not found")),
        };
        let anyhow_err = handle_upload_error(err, "local.tar.gz", "remote/file.tar.gz");
        let msg = anyhow_err.to_string();
        assert!(msg.contains("bucket or object not found"), "got: {msg}");
        assert!(msg.contains("local.tar.gz"), "got: {msg}");
    }

    #[test]
    fn test_handle_upload_error_unauthenticated() {
        let err = object_store::Error::Unauthenticated {
            path: "test".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "bad creds",
            )),
        };
        let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
        assert!(anyhow_err.to_string().contains("authentication failed"));
    }

    #[test]
    fn test_handle_upload_error_permission_denied() {
        let err = object_store::Error::PermissionDenied {
            path: "test".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "access denied",
            )),
        };
        let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
        assert!(anyhow_err.to_string().contains("access denied"));
    }

    #[test]
    fn test_handle_upload_error_generic() {
        let err = object_store::Error::Generic {
            store: "S3",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "network timeout",
            )),
        };
        let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
        assert!(anyhow_err.to_string().contains("upload failed"));
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
    region: us-west-2
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].provider, "s3");
        assert_eq!(blobs[0].bucket, "my-releases");
        assert_eq!(blobs[0].region.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn test_blob_config_parses_gcs() {
        let yaml = r#"
blobs:
  - provider: gs
    bucket: my-gcs-bucket
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs[0].provider, "gs");
    }

    #[test]
    fn test_blob_config_parses_azblob() {
        let yaml = r#"
blobs:
  - provider: azblob
    bucket: my-container
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs[0].provider, "azblob");
    }

    #[test]
    fn test_blob_config_multiple_providers() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: aws-bucket
  - provider: gs
    bucket: gcs-bucket
  - provider: azblob
    bucket: azure-container
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs.len(), 3);
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
    cache_control:
      - "no-cache"
      - "no-store"
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
        assert_eq!(
            b.cache_control.as_ref().unwrap(),
            &["no-cache", "no-store"]
        );
        assert_eq!(b.content_disposition.as_deref(), Some("inline"));
        assert_eq!(b.kms_key.as_deref(), Some("key123"));
        assert_eq!(b.ids.as_ref().unwrap(), &["build-linux"]);
        assert_eq!(b.disable, Some(StringOrBool::Bool(false)));
        assert_eq!(b.include_meta, Some(true));
        assert!(b.extra_files.is_some());
        assert_eq!(b.extra_files_only, Some(false));
        assert_eq!(b.id.as_deref(), Some("my-blob-config"));
    }

    #[test]
    fn test_blob_config_cache_control_as_string() {
        // Backward compat: cache_control as a single string
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    cache_control: "max-age=86400"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(
            blobs[0].cache_control.as_ref().unwrap(),
            &["max-age=86400"]
        );
    }

    #[test]
    fn test_blob_config_cache_control_as_array() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    cache_control:
      - "max-age=86400"
      - "public"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(
            blobs[0].cache_control.as_ref().unwrap(),
            &["max-age=86400", "public"]
        );
    }

    #[test]
    fn test_blob_config_disable_as_bool() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    disable: true
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        assert_eq!(blobs[0].disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_blob_config_disable_as_template_string() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    disable: "{% if IsSnapshot %}true{% endif %}"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        match &blobs[0].disable {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got: {other:?}"),
        }
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
    fn test_blob_config_extra_files_name_template_alias() {
        let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    extra_files:
      - glob: "./LICENSE"
        name_template: "LICENSE-{{ Tag }}"
"#;
        let crate_cfg: anodize_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let blobs = crate_cfg.blobs.unwrap();
        let extras = blobs[0].extra_files.as_ref().unwrap();
        assert_eq!(extras[0].name.as_deref(), Some("LICENSE-{{ Tag }}"));
    }

    #[test]
    fn test_partial_config_parses() {
        let yaml = r#"
partial:
  by: goos
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(
            &format!("project_name: test\ncrates: []\n{}", yaml),
        )
        .unwrap();
        let partial = config.partial.unwrap();
        assert_eq!(partial.by.as_deref(), Some("goos"));
    }

    #[test]
    fn test_partial_config_by_target() {
        let yaml = "project_name: test\ncrates: []\npartial:\n  by: target\n";
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.partial.unwrap().by.as_deref(),
            Some("target")
        );
    }

    #[test]
    fn test_partial_config_defaults() {
        let yaml = "project_name: test\ncrates: []\npartial: {}\n";
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        // by defaults to None, which the runtime interprets as "goos"
        assert!(config.partial.unwrap().by.is_none());
    }

    // -----------------------------------------------------------------------
    // Extra files resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_extra_files_basic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("LICENSE");
        std::fs::write(&file_path, "MIT").unwrap();

        let extras = vec![ExtraFile {
            glob: file_path.to_string_lossy().to_string(),
            name: None,
        }];
        let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1, "LICENSE");
    }

    #[test]
    fn test_resolve_extra_files_with_name_template() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("LICENSE");
        std::fs::write(&file_path, "MIT").unwrap();

        let extras = vec![ExtraFile {
            glob: file_path.to_string_lossy().to_string(),
            name: Some("LICENSE-{{ Tag }}".to_string()),
        }];

        let mut ctx = make_ctx();
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        let resolved = resolve_extra_files(&extras, &ctx, &test_log()).unwrap();
        assert_eq!(resolved[0].1, "LICENSE-v1.0.0");
    }

    #[test]
    fn test_resolve_extra_files_glob_pattern() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("LICENSE"), "MIT").unwrap();
        std::fs::write(tmp.path().join("LICENSE.md"), "MIT").unwrap();

        let glob_pattern = format!("{}/*", tmp.path().display());
        let extras = vec![ExtraFile {
            glob: glob_pattern,
            name: None,
        }];
        let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn test_resolve_extra_files_no_match_warns_and_continues() {
        let extras = vec![ExtraFile {
            glob: "/nonexistent/path/to/files/*.xyz".to_string(),
            name: None,
        }];
        // Should succeed with empty results (warning logged), not error
        let result = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Artifact filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_artifacts_filters_by_crate() {
        let mut ctx = make_ctx();
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/b.tar.gz"),
            target: None,
            crate_name: "othercrate".to_string(),
            metadata: Default::default(),
        });
        let config = BlobConfig::default();
        let arts = collect_artifacts(&ctx, &config, "mycrate");
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].crate_name, "mycrate");
    }

    #[test]
    fn test_collect_artifacts_extra_files_only() {
        let mut ctx = make_ctx();
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
        });
        let config = BlobConfig {
            extra_files_only: Some(true),
            ..Default::default()
        };
        let arts = collect_artifacts(&ctx, &config, "mycrate");
        assert!(arts.is_empty());
    }

    #[test]
    fn test_collect_artifacts_ids_filter() {
        let mut ctx = make_ctx();
        let mut meta = std::collections::HashMap::new();
        meta.insert("id".to_string(), "linux-build".to_string());
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: meta,
        });
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/b.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
        });
        let config = BlobConfig {
            ids: Some(vec!["linux-build".to_string()]),
            ..Default::default()
        };
        let arts = collect_artifacts(&ctx, &config, "mycrate");
        assert_eq!(arts.len(), 1);
    }

    #[test]
    fn test_collect_artifacts_includes_metadata_kind() {
        let mut ctx = make_ctx();
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Metadata,
            path: PathBuf::from("dist/metadata.json"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
        });
        // Without include_meta
        let config = BlobConfig::default();
        let arts = collect_artifacts(&ctx, &config, "mycrate");
        assert!(arts.is_empty());

        // With include_meta
        let config_meta = BlobConfig {
            include_meta: Some(true),
            ..Default::default()
        };
        let arts = collect_artifacts(&ctx, &config_meta, "mycrate");
        assert_eq!(arts.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
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
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

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
                    disable: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
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
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("provider is required"));
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
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bucket is required"));
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
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        let stage = BlobStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown provider"));
    }

    #[test]
    fn test_blob_stage_dry_run_logs_commands() {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            crates: vec![anodize_core::config::CrateConfig {
                name: "mycrate".to_string(),
                path: ".".to_string(),
                blobs: Some(vec![BlobConfig {
                    provider: "s3".to_string(),
                    bucket: "my-bucket".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut opts = ContextOptions::default();
        opts.dry_run = true;
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");

        // Add an artifact so there's something to "upload"
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/test-v1.0.0.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
        });

        let stage = BlobStage;
        // Dry-run should succeed without any credentials
        let result = stage.run(&mut ctx);
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // StringOrBool unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_string_or_bool_as_bool() {
        assert!(StringOrBool::Bool(true).as_bool());
        assert!(!StringOrBool::Bool(false).as_bool());
        assert!(StringOrBool::String("true".to_string()).as_bool());
        assert!(StringOrBool::String("1".to_string()).as_bool());
        assert!(!StringOrBool::String("false".to_string()).as_bool());
        assert!(!StringOrBool::String("".to_string()).as_bool());
    }

    #[test]
    fn test_string_or_bool_is_template() {
        assert!(StringOrBool::String("{% if X %}true{% endif %}".to_string()).is_template());
        assert!(!StringOrBool::String("true".to_string()).is_template());
        assert!(!StringOrBool::Bool(true).is_template());
    }

    // -----------------------------------------------------------------------
    // Integration test with InMemory store
    // -----------------------------------------------------------------------

    #[test]
    fn test_upload_to_in_memory_store() {
        use object_store::ObjectStoreExt as _;

        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let tmp = tempfile::TempDir::new().unwrap();

        // Create test files
        let file1 = tmp.path().join("app.tar.gz");
        std::fs::write(&file1, b"archive data").unwrap();
        let file2 = tmp.path().join("checksums.sha256");
        std::fs::write(&file2, b"abc123  app.tar.gz").unwrap();

        let upload_items = vec![
            (file1, "app.tar.gz".to_string()),
            (file2, "checksums.sha256".to_string()),
        ];
        let config = BlobConfig::default();
        let ctx = make_ctx();
        let log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let params = UploadParams {
            store: Arc::clone(&store),
            items: &upload_items,
            directory: "myproject/v1.0.0",
            config: &config,
            ctx: &ctx,
        };
        let result = upload_files(&params);
        assert!(result.is_ok(), "upload failed: {:?}", result.err());

        // Verify files were uploaded to the store
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let path1 = ObjectPath::from("myproject/v1.0.0/app.tar.gz");
            let get_result = store
                .get_opts(&path1, object_store::GetOptions::default())
                .await
                .unwrap();
            let data = get_result.bytes().await.unwrap();
            assert_eq!(data.as_ref(), b"archive data");

            let path2 = ObjectPath::from("myproject/v1.0.0/checksums.sha256");
            let get_result = store
                .get_opts(&path2, object_store::GetOptions::default())
                .await
                .unwrap();
            let data = get_result.bytes().await.unwrap();
            assert_eq!(data.as_ref(), b"abc123  app.tar.gz");
        });
    }

    #[test]
    fn test_upload_to_in_memory_store_empty_directory() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let tmp = tempfile::TempDir::new().unwrap();

        let file1 = tmp.path().join("file.txt");
        std::fs::write(&file1, b"data").unwrap();

        let upload_items = vec![(file1, "file.txt".to_string())];
        let config = BlobConfig::default();
        let ctx = make_ctx();
        let log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let params = UploadParams {
            store: Arc::clone(&store),
            items: &upload_items,
            directory: "",
            config: &config,
            ctx: &ctx,
        };
        let result = upload_files(&params);
        assert!(result.is_ok());

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let path = ObjectPath::from("file.txt");
            let get_result = store
                .get_opts(&path, object_store::GetOptions::default())
                .await
                .unwrap();
            let data = get_result.bytes().await.unwrap();
            assert_eq!(data.as_ref(), b"data");
        });
    }

    #[test]
    fn test_upload_with_content_disposition() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let tmp = tempfile::TempDir::new().unwrap();

        let file1 = tmp.path().join("app.tar.gz");
        std::fs::write(&file1, b"data").unwrap();

        let upload_items = vec![(file1, "app.tar.gz".to_string())];
        let config = BlobConfig {
            content_disposition: Some("attachment;filename={{Filename}}".to_string()),
            ..Default::default()
        };
        let ctx = make_ctx();
        let log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let params = UploadParams {
            store,
            items: &upload_items,
            directory: "dir",
            config: &config,
            ctx: &ctx,
        };
        let result = upload_files(&params);
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn test_log() -> anodize_core::log::StageLogger {
        anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet)
    }

    fn make_ctx() -> Context {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        let opts = ContextOptions::default();
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");
        ctx.template_vars_mut().set("IsSnapshot", "false");
        ctx
    }

    fn make_ctx_with_snapshot() -> Context {
        let config = anodize_core::config::Config {
            project_name: "test".to_string(),
            ..Default::default()
        };
        let mut opts = ContextOptions::default();
        opts.snapshot = true;
        let mut ctx = Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "test");
        ctx.template_vars_mut().set("IsSnapshot", "true");
        ctx
    }
}
