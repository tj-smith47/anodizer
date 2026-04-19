use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;

use anodize_core::artifact::{Artifact, ArtifactKind, release_uploadable_kinds};
use anodize_core::config::{BlobConfig, ExtraFileSpec};
use anodize_core::context::Context;
use anodize_core::extrafiles;
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
// KMS provider detection and client-side encryption
// ---------------------------------------------------------------------------

/// Identifies how the KMS key should be used for encryption.
///
/// GoReleaser supports `awskms://`, `gcpkms://`, and `azurekeyvault://` URL
/// schemes via gocloud.dev/secrets for **client-side** encryption of blob data
/// before upload. A plain key ARN/ID (no scheme) means server-side encryption
/// (SSE-KMS on S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KmsProvider {
    /// `awskms://key-id` or `awskms:///arn:aws:kms:...` — client-side via AWS CLI
    Aws,
    /// `gcpkms://projects/.../cryptKeys/...` — client-side via gcloud CLI
    Gcp,
    /// `azurekeyvault://vault-name/keys/key-name[/version]` — client-side via az CLI
    Azure,
    /// Plain key ARN/ID without URL scheme — server-side SSE-KMS (S3 only)
    ServerSide,
}

fn parse_kms_provider(kms_key: &str) -> KmsProvider {
    if kms_key.starts_with("awskms://") {
        KmsProvider::Aws
    } else if kms_key.starts_with("gcpkms://") {
        KmsProvider::Gcp
    } else if kms_key.starts_with("azurekeyvault://") {
        KmsProvider::Azure
    } else {
        KmsProvider::ServerSide
    }
}

/// Encrypt `data` client-side using the appropriate cloud CLI tool.
///
/// Returns the encrypted ciphertext bytes. For `ServerSide`, returns the data
/// unchanged — the S3 builder handles SSE-KMS configuration at the transport
/// level.
fn encrypt_with_kms(data: &[u8], kms_key: &str, provider: KmsProvider) -> Result<Vec<u8>> {
    match provider {
        KmsProvider::Aws => {
            // awskms://key-id  or  awskms:///arn:aws:kms:region:account:key/id
            let key_id = kms_key
                .strip_prefix("awskms://")
                .ok_or_else(|| anyhow::anyhow!("expected awskms:// scheme, got {kms_key}"))?
                .trim_start_matches('/');

            let mut child = std::process::Command::new("aws")
                .args([
                    "kms",
                    "encrypt",
                    "--key-id",
                    key_id,
                    "--plaintext",
                    "fileb:///dev/stdin",
                    "--output",
                    "json",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("failed to run 'aws' CLI — is it installed?")?;

            child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("blobs: aws kms child has no stdin"))?
                .write_all(data)
                .context("blobs: failed to write plaintext to aws kms stdin")?;

            let output = child
                .wait_with_output()
                .context("blobs: failed to wait for aws kms encrypt")?;

            if !output.status.success() {
                bail!(
                    "aws kms encrypt failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let resp: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("blobs: failed to parse aws kms encrypt JSON response")?;
            let b64 = resp["CiphertextBlob"]
                .as_str()
                .context("missing CiphertextBlob in aws kms encrypt response")?;

            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("blobs: failed to decode CiphertextBlob base64")
        }

        KmsProvider::Gcp => {
            // gcpkms://projects/PROJECT/locations/LOC/keyRings/KR/cryptKeys/KEY
            let resource = kms_key
                .strip_prefix("gcpkms://")
                .ok_or_else(|| anyhow::anyhow!("expected gcpkms:// scheme, got {kms_key}"))?;

            let mut child = std::process::Command::new("gcloud")
                .args([
                    "kms",
                    "encrypt",
                    "--key",
                    resource,
                    "--plaintext-file",
                    "-",
                    "--ciphertext-file",
                    "-",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("failed to run 'gcloud' CLI — is it installed?")?;

            child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("blobs: gcloud kms child has no stdin"))?
                .write_all(data)
                .context("blobs: failed to write plaintext to gcloud kms stdin")?;

            let output = child
                .wait_with_output()
                .context("blobs: failed to wait for gcloud kms encrypt")?;

            if !output.status.success() {
                bail!(
                    "gcloud kms encrypt failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            // gcloud outputs raw ciphertext bytes to stdout
            Ok(output.stdout)
        }

        KmsProvider::Azure => {
            // azurekeyvault://vault-name/keys/key-name[/version]
            let path = kms_key.strip_prefix("azurekeyvault://").ok_or_else(|| {
                anyhow::anyhow!("expected azurekeyvault:// scheme, got {kms_key}")
            })?;
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            let vault_name = parts
                .first()
                .context("missing vault name in azurekeyvault:// URL")?;
            // parts[1] is "keys", parts[2] is "key-name[/version]"
            let key_name = parts
                .get(2)
                .context("missing key name in azurekeyvault:// URL (expected vault/keys/name)")?;

            let b64_data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data);

            let output = std::process::Command::new("az")
                .args([
                    "keyvault",
                    "key",
                    "encrypt",
                    "--vault-name",
                    vault_name,
                    "--name",
                    key_name,
                    "--algorithm",
                    "RSA-OAEP-256",
                    "--value",
                    &b64_data,
                    "--output",
                    "json",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to run 'az' CLI — is it installed?")?;

            if !output.status.success() {
                bail!(
                    "az keyvault key encrypt failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let resp: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("blobs: failed to parse az keyvault encrypt JSON response")?;
            let result = resp["result"]
                .as_str()
                .context("missing 'result' field in az keyvault encrypt response")?;

            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(result)
                .context("blobs: failed to decode az keyvault encryption result")
        }

        KmsProvider::ServerSide => {
            // Not client-side encryption — return data unchanged.
            // The S3 builder handles SSE-KMS at the transport level.
            Ok(data.to_vec())
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

    // KMS server-side encryption: only set SSE-KMS on the S3 builder when the
    // key is a plain ARN/ID (ServerSide). URL-schemed keys (awskms://, gcpkms://,
    // azurekeyvault://) use client-side encryption — the data is encrypted before
    // upload, so we must NOT also request server-side encryption.
    if let Some(ref kms_key) = config.kms_key
        && parse_kms_provider(kms_key) == KmsProvider::ServerSide
    {
        builder = builder.with_sse_kms_encryption(kms_key);
    }

    // S3 canned ACL via x-amz-acl header.
    // We set it as a default header on the client — since each blob config
    // gets its own ObjectStore client, this is per-config ACL.
    if let Some(ref acl) = config.acl {
        // Validate against the S3 canned ACL enum. Matches GoReleaser
        // internal/pipe/blob/upload.go:113-119 exactly — `log-delivery-write`
        // (a valid AWS S3 canned ACL) is omitted to match upstream.
        const VALID_S3_ACLS: &[&str] = &[
            "private",
            "public-read",
            "public-read-write",
            "authenticated-read",
            "aws-exec-read",
            "bucket-owner-read",
            "bucket-owner-full-control",
        ];
        if !VALID_S3_ACLS.contains(&acl.as_str()) {
            anyhow::bail!(
                "blobs: invalid S3 canned ACL '{}'. Valid values are: {}",
                acl,
                VALID_S3_ACLS.join(", ")
            );
        }

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

fn build_put_options(config: &BlobConfig, filename: &str, ctx: &Context) -> Result<PutOptions> {
    use object_store::Attribute;

    let mut attrs = object_store::Attributes::new();

    // Cache-Control: join array with ", " (GoReleaser uses []string)
    if let Some(ref cc) = config.cache_control
        && !cc.is_empty()
    {
        attrs.insert(Attribute::CacheControl, cc.join(", ").into());
    }

    // Content-Disposition: only set when user provides a non-empty value.
    // GoReleaser never force-defaults `attachment;filename=...` — letting the
    // backend default preserves in-browser preview for images/PDFs/HTML.
    // Sentinel `"-"` disables the header explicitly (kept for parity with users
    // migrating from earlier anodize configs).
    if let Some(disp_template) = config.content_disposition.as_deref()
        && !disp_template.is_empty()
        && disp_template != "-"
    {
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

// ---------------------------------------------------------------------------
// Extra files resolution — with template-rendered names
// ---------------------------------------------------------------------------

fn resolve_extra_files(
    extra_files: &[ExtraFileSpec],
    ctx: &Context,
    log: &anodize_core::log::StageLogger,
) -> Result<Vec<(PathBuf, String)>> {
    let resolved = extrafiles::resolve(extra_files, log)?;
    let mut out = Vec::with_capacity(resolved.len());
    for entry in resolved {
        let upload_name = if let Some(ref name_tmpl) = entry.name_template {
            let filename = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let mut vars = ctx.template_vars().clone();
            vars.set("Filename", filename);
            template::render(name_tmpl, &vars)
                .with_context(|| format!("blobs: render extra_files name: {name_tmpl}"))?
        } else {
            entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string()
        };
        out.push((entry.path, upload_name));
    }
    Ok(out)
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

    // blob upload uses the canonical
    // release-uploadable artifact list (ArtifactByType(Archive, UploadableBinary,
    // UploadableFile, SourceArchive, Makeself, LinuxPackage, Flatpak, SourceRpm,
    // Sbom, Checksum, Signature, Certificate)). When
    // `include_meta` is true, append Metadata.
    let mut uploadable_kinds: Vec<ArtifactKind> = release_uploadable_kinds().to_vec();
    if config.include_meta.unwrap_or(false) {
        uploadable_kinds.push(ArtifactKind::Metadata);
    }

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

/// Upload a per-config batch of files with intra-config parallelism via
/// tokio, given fully owned data. Phase 2 of `BlobStage::run` calls this
/// on worker threads so `ctx` is never touched after Phase 1.
fn upload_files_owned(
    store: Arc<dyn ObjectStore>,
    items: Vec<(PathBuf, String)>,
    directory: String,
    put_opts_per_item: Vec<PutOptions>,
    parallelism: usize,
    client_kms: Option<(String, KmsProvider)>,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("blobs: failed to create tokio runtime")?;
    rt.block_on(async move {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism.max(1)));
        let mut handles = Vec::new();

        for ((local_path, remote_key), put_opts) in items.into_iter().zip(put_opts_per_item) {
            let dir_trimmed = directory.trim_matches('/');
            let object_key = if dir_trimmed.is_empty() {
                remote_key.clone()
            } else {
                format!("{}/{}", dir_trimmed, remote_key)
            };

            let object_path = ObjectPath::from(object_key.as_str());

            let store = Arc::clone(&store);
            let sem = Arc::clone(&semaphore);
            let path_display = local_path.display().to_string();
            let local = local_path;
            let key_display = object_key.clone();
            let client_kms = client_kms.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("semaphore error: {}", e))?;
                let data = tokio::fs::read(&local).await.map_err(|e| {
                    anyhow::anyhow!("blobs: read file for upload: {}: {}", path_display, e)
                })?;

                let upload_data = if let Some((kms_key, provider)) = client_kms {
                    tokio::task::spawn_blocking(move || encrypt_with_kms(&data, &kms_key, provider))
                        .await
                        .map_err(|e| anyhow::anyhow!("KMS encryption task panicked: {}", e))??
                } else {
                    data
                };

                store
                    .put_opts(&object_path, upload_data.into(), put_opts)
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

fn handle_upload_error(
    err: object_store::Error,
    local_path: &str,
    remote_key: &str,
) -> anyhow::Error {
    match &err {
        object_store::Error::NotFound { path, .. } => {
            anyhow::anyhow!(
                "blobs: bucket or object not found ({}): uploading {} -> {}",
                path,
                local_path,
                remote_key
            )
        }
        object_store::Error::Unauthenticated { path, .. } => {
            anyhow::anyhow!(
                "blobs: authentication failed — check credentials. Uploading {} -> {} ({})",
                local_path,
                remote_key,
                path
            )
        }
        object_store::Error::PermissionDenied { path, .. } => {
            anyhow::anyhow!(
                "blobs: access denied — check permissions. Uploading {} -> {} ({})",
                local_path,
                remote_key,
                path
            )
        }
        _ => {
            anyhow::anyhow!(
                "blobs: upload failed for {} -> {}: {}",
                local_path,
                remote_key,
                err
            )
        }
    }
}

// ---------------------------------------------------------------------------
// BlobStage
// ---------------------------------------------------------------------------

pub struct BlobStage;

/// A fully-prepared blob upload job. Phase 1 (serial, `&mut ctx`) renders
/// templates, builds the ObjectStore, pre-renders per-item put options;
/// Phase 2 (parallel) runs the per-config upload via `upload_files_owned`.
/// Workers never touch `ctx`.
struct BlobJob {
    provider_display: &'static str,
    rendered_bucket: String,
    rendered_directory: String,
    upload_items: Vec<(PathBuf, String)>,
    store: Arc<dyn ObjectStore>,
    put_opts_per_item: Vec<PutOptions>,
    parallelism_inner: usize,
    client_kms: Option<(String, KmsProvider)>,
}

impl Stage for BlobStage {
    fn name(&self) -> &str {
        "blob"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("blob");
        if ctx.skip_in_snapshot(&log, "blob") {
            return Ok(());
        }

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let global_parallelism = ctx.options.parallelism.max(1);

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

        // Phase 1 (serial): render every config, build stores, collect jobs.
        let mut jobs: Vec<BlobJob> = Vec::new();

        for krate in &crates {
            // SAFETY: `crates` was filtered to only include crates with
            // `blobs.is_some()` above, so this Option is always Some here.
            // `continue` defends against a future refactor that breaks the
            // invariant rather than panicking on the now-impossible None.
            let Some(blob_configs) = krate.blobs.as_ref() else {
                continue;
            };

            for blob_cfg in blob_configs {
                // Evaluate disable (supports both bool and template string)
                if ctx.is_disabled_with_log(
                    &blob_cfg.disable,
                    &log,
                    &format!("blob config for crate {}", krate.name),
                ) {
                    continue;
                }

                // Validate required fields
                if blob_cfg.provider.is_empty() {
                    anyhow::bail!("blobs: provider is required for crate '{}'", krate.name);
                }
                if blob_cfg.bucket.is_empty() {
                    anyhow::bail!("blobs: bucket is required for crate '{}'", krate.name);
                }

                let provider_str = ctx.render_template(&blob_cfg.provider).with_context(|| {
                    format!(
                        "blobs: render provider template '{}' for crate '{}'",
                        blob_cfg.provider, krate.name
                    )
                })?;
                let provider = Provider::parse(&provider_str)?;
                let config_label = blob_cfg.id.as_deref().unwrap_or(&provider_str);

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

                // Process templated_extra_files: render and add to upload list.
                // NOTE: Rendered files are written to the shared dist directory. If multiple
                // blob configs use the same dst name, later writes will overwrite earlier
                // ones. Users should ensure dst names are unique across configs.
                if let Some(ref tpl_specs) = blob_cfg.templated_extra_files
                    && !tpl_specs.is_empty()
                {
                    let rendered = anodize_core::templated_files::process_templated_extra_files(
                        tpl_specs,
                        ctx,
                        &ctx.config.dist,
                        "blobs",
                    )?;
                    upload_items.extend(rendered);
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
                    continue;
                }

                // Log each file before upload (serial stays in Phase 1 so
                // the per-config announcement order remains deterministic,
                // matching the pre-parallel behaviour).
                for (local_path, remote_key) in &upload_items {
                    let remote = format_remote_path(
                        provider,
                        &rendered_bucket,
                        &rendered_directory,
                        remote_key,
                    );
                    log.status(&format!("uploading {} -> {}", local_path.display(), remote));
                }

                let store: Arc<dyn ObjectStore> =
                    Arc::from(build_store(provider, blob_cfg, &rendered_bucket, ctx)?);

                // Pre-render put options per item while we still hold &ctx.
                let put_opts_per_item: Vec<PutOptions> = upload_items
                    .iter()
                    .map(|(_, key)| build_put_options(blob_cfg, key, ctx))
                    .collect::<Result<_>>()?;

                // Determine if client-side KMS encryption is needed
                let client_kms = blob_cfg.kms_key.as_deref().and_then(|key| {
                    let kms_provider = parse_kms_provider(key);
                    match kms_provider {
                        KmsProvider::ServerSide => None,
                        _ => Some((key.to_string(), kms_provider)),
                    }
                });

                let parallelism_inner = blob_cfg
                    .parallelism
                    .unwrap_or(ctx.options.parallelism)
                    .max(1);

                jobs.push(BlobJob {
                    provider_display: provider.display_name(),
                    rendered_bucket,
                    rendered_directory,
                    upload_items,
                    store,
                    put_opts_per_item,
                    parallelism_inner,
                    client_kms,
                });
            }
        }

        if jobs.is_empty() {
            return Ok(());
        }

        // Phase 2 (parallel across configs): each worker runs its own
        // upload loop (which itself has intra-config per-file concurrency
        // via tokio). Bounded by the global parallelism so we don't fan
        // out unbounded across both axes simultaneously.
        let run_job = |job: &BlobJob| -> Result<()> {
            upload_files_owned(
                Arc::clone(&job.store),
                job.upload_items.clone(),
                job.rendered_directory.clone(),
                job.put_opts_per_item.clone(),
                job.parallelism_inner,
                job.client_kms.clone(),
            )
        };

        anodize_core::parallel::run_parallel_chunks(&jobs, global_parallelism, "blob", run_job)?;

        for job in &jobs {
            log.status(&format!(
                "uploaded {} file(s) to {} {}/{}",
                job.upload_items.len(),
                job.provider_display,
                job.rendered_bucket,
                job.rendered_directory,
            ));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{BlobConfig, StringOrBool};
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
            cache_control: Some(vec!["max-age=86400".to_string(), "public".to_string()]),
            ..Default::default()
        };
        let opts = build_put_options(&config, "file.tar.gz", &make_ctx()).unwrap();
        let cc = opts
            .attributes
            .get(&object_store::Attribute::CacheControl)
            .unwrap_or_else(|| panic!("cache_control should be set"));
        assert_eq!(cc.as_ref(), "max-age=86400, public");
    }

    #[test]
    fn test_put_options_cache_control_single() {
        let config = BlobConfig {
            cache_control: Some(vec!["no-cache".to_string()]),
            ..Default::default()
        };
        let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
        let cc = opts
            .attributes
            .get(&object_store::Attribute::CacheControl)
            .unwrap_or_else(|| panic!("should be set"));
        assert_eq!(cc.as_ref(), "no-cache");
    }

    #[test]
    fn test_put_options_content_disposition_default_unset() {
        // B3 fix: GoReleaser does NOT default content-disposition; anodize
        // must not either, so in-browser preview (images/PDFs/HTML) keeps
        // working when users don't opt into attachment behavior.
        let config = BlobConfig::default();
        let opts = build_put_options(&config, "myapp-v1.tar.gz", &make_ctx()).unwrap();
        assert!(
            opts.attributes
                .get(&object_store::Attribute::ContentDisposition)
                .is_none(),
            "default content-disposition must be unset (matches GoReleaser)"
        );
    }

    #[test]
    fn test_put_options_content_disposition_disabled() {
        let config = BlobConfig {
            content_disposition: Some("-".to_string()),
            ..Default::default()
        };
        let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
        assert!(
            opts.attributes
                .get(&object_store::Attribute::ContentDisposition)
                .is_none()
        );
    }

    #[test]
    fn test_put_options_content_disposition_custom() {
        let config = BlobConfig {
            content_disposition: Some("inline".to_string()),
            ..Default::default()
        };
        let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
        let cd = opts
            .attributes
            .get(&object_store::Attribute::ContentDisposition)
            .unwrap_or_else(|| panic!("should be set"));
        assert_eq!(cd.as_ref(), "inline");
    }

    // -----------------------------------------------------------------------
    // Disable evaluation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_disabled_none() {
        assert!(!make_ctx().is_disabled_with_log(&None, &test_log(), "t"));
    }

    #[test]
    fn test_is_disabled_bool_true() {
        assert!(make_ctx().is_disabled_with_log(&Some(StringOrBool::Bool(true)), &test_log(), "t"));
    }

    #[test]
    fn test_is_disabled_bool_false() {
        assert!(!make_ctx().is_disabled_with_log(
            &Some(StringOrBool::Bool(false)),
            &test_log(),
            "t"
        ));
    }

    #[test]
    fn test_is_disabled_string_true() {
        assert!(make_ctx().is_disabled_with_log(
            &Some(StringOrBool::String("true".to_string())),
            &test_log(),
            "t"
        ));
    }

    #[test]
    fn test_is_disabled_string_false() {
        assert!(!make_ctx().is_disabled_with_log(
            &Some(StringOrBool::String("false".to_string())),
            &test_log(),
            "t"
        ));
    }

    #[test]
    fn test_is_disabled_template_evaluates_to_true() {
        let ctx = make_ctx_with_snapshot();
        let disable = Some(StringOrBool::String(
            "{% if IsSnapshot %}true{% endif %}".to_string(),
        ));
        assert!(ctx.is_disabled_with_log(&disable, &test_log(), "t"));
    }

    #[test]
    fn test_is_disabled_template_evaluates_to_false() {
        let ctx = make_ctx(); // IsSnapshot is "false"
        let disable = Some(StringOrBool::String(
            "{% if IsSnapshot == \"true\" %}true{% endif %}".to_string(),
        ));
        assert!(!ctx.is_disabled_with_log(&disable, &test_log(), "t"));
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
    // KMS provider detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_kms_provider_aws() {
        assert_eq!(
            parse_kms_provider("awskms://alias/my-key"),
            KmsProvider::Aws
        );
        assert_eq!(
            parse_kms_provider("awskms:///arn:aws:kms:us-east-1:123456:key/abc-def"),
            KmsProvider::Aws
        );
    }

    #[test]
    fn test_parse_kms_provider_gcp() {
        assert_eq!(
            parse_kms_provider(
                "gcpkms://projects/my-proj/locations/global/keyRings/kr/cryptKeys/key"
            ),
            KmsProvider::Gcp
        );
    }

    #[test]
    fn test_parse_kms_provider_azure() {
        assert_eq!(
            parse_kms_provider("azurekeyvault://my-vault/keys/my-key"),
            KmsProvider::Azure
        );
        assert_eq!(
            parse_kms_provider("azurekeyvault://my-vault/keys/my-key/version1"),
            KmsProvider::Azure
        );
    }

    #[test]
    fn test_parse_kms_provider_server_side() {
        // Plain ARN — no URL scheme
        assert_eq!(
            parse_kms_provider("arn:aws:kms:us-east-1:123456:key/abc-def"),
            KmsProvider::ServerSide
        );
        // Plain key ID
        assert_eq!(
            parse_kms_provider("1234abcd-12ab-34cd-56ef-1234567890ab"),
            KmsProvider::ServerSide
        );
        // Alias without scheme
        assert_eq!(parse_kms_provider("alias/my-key"), KmsProvider::ServerSide);
    }

    #[test]
    fn test_encrypt_with_kms_server_side_passthrough() {
        // ServerSide should return data unchanged
        let data = b"hello world";
        let result = encrypt_with_kms(data, "some-key-arn", KmsProvider::ServerSide).unwrap();
        assert_eq!(result, data);
    }

    // -----------------------------------------------------------------------
    // Error handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_upload_error_not_found() {
        let err = object_store::Error::NotFound {
            path: "test/path".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not found",
            )),
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
            source: Box::new(std::io::Error::other("network timeout")),
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
        assert_eq!(b.cache_control.as_ref().unwrap(), &["no-cache", "no-store"]);
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
        assert_eq!(blobs[0].cache_control.as_ref().unwrap(), &["max-age=86400"]);
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
        assert_eq!(extras[0].glob(), "./LICENSE");
        assert_eq!(extras[0].name_template(), Some("LICENSE.txt"));
        assert_eq!(extras[1].glob(), "./README.md");
        assert!(extras[1].name_template().is_none());
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
        assert_eq!(extras[0].name_template(), Some("LICENSE-{{ Tag }}"));
    }

    #[test]
    fn test_partial_config_parses() {
        let yaml = r#"
partial:
  by: goos
"#;
        let config: anodize_core::config::Config =
            serde_yaml_ng::from_str(&format!("project_name: test\ncrates: []\n{}", yaml)).unwrap();
        let partial = config.partial.unwrap();
        assert_eq!(partial.by.as_deref(), Some("goos"));
    }

    #[test]
    fn test_partial_config_by_target() {
        let yaml = "project_name: test\ncrates: []\npartial:\n  by: target\n";
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.partial.unwrap().by.as_deref(), Some("target"));
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

        let extras = vec![ExtraFileSpec::Glob(file_path.to_string_lossy().to_string())];
        let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1, "LICENSE");
    }

    #[test]
    fn test_resolve_extra_files_with_name_template() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("LICENSE");
        std::fs::write(&file_path, "MIT").unwrap();

        let extras = vec![ExtraFileSpec::Detailed {
            glob: file_path.to_string_lossy().to_string(),
            name_template: Some("LICENSE-{{ Tag }}".to_string()),
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
        let extras = vec![ExtraFileSpec::Glob(glob_pattern)];
        let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn test_resolve_extra_files_no_match_warns_and_continues() {
        let extras = vec![ExtraFileSpec::Glob(
            "/nonexistent/path/to/files/*.xyz".to_string(),
        )];
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
            name: String::new(),
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/b.tar.gz"),
            target: None,
            crate_name: "othercrate".to_string(),
            metadata: Default::default(),
            size: None,
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
            name: String::new(),
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
            size: None,
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
            name: String::new(),
            path: PathBuf::from("dist/a.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: meta,
            size: None,
        });
        ctx.artifacts.add(anodize_core::artifact::Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/b.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
            size: None,
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
            name: String::new(),
            path: PathBuf::from("dist/metadata.json"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
            size: None,
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("provider is required")
        );
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("bucket is required")
        );
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
            name: String::new(),
            path: PathBuf::from("dist/test-v1.0.0.tar.gz"),
            target: None,
            crate_name: "mycrate".to_string(),
            metadata: Default::default(),
            size: None,
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
        let _log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let put_opts: Vec<PutOptions> = upload_items
            .iter()
            .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
            .collect();
        let result = upload_files_owned(
            Arc::clone(&store),
            upload_items.clone(),
            "myproject/v1.0.0".to_string(),
            put_opts,
            1,
            None,
        );
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
        let _log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let put_opts: Vec<PutOptions> = upload_items
            .iter()
            .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
            .collect();
        let result = upload_files_owned(
            Arc::clone(&store),
            upload_items.clone(),
            String::new(),
            put_opts,
            1,
            None,
        );
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
        let _log = anodize_core::log::StageLogger::new("test", anodize_core::log::Verbosity::Quiet);

        let put_opts: Vec<PutOptions> = upload_items
            .iter()
            .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
            .collect();
        let result = upload_files_owned(
            store,
            upload_items.clone(),
            "dir".to_string(),
            put_opts,
            1,
            None,
        );
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
