use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind, release_uploadable_kinds};
use anodizer_core::config::{BlobConfig, ExtraFileSpec};
use anodizer_core::context::Context;
use anodizer_core::extrafiles;
use anodizer_core::template;

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutOptions};

use crate::kms::{KmsProvider, encrypt_with_kms};
use crate::provider::Provider;

// ---------------------------------------------------------------------------
// Put options — headers (cache-control, content-disposition)
// ---------------------------------------------------------------------------

/// Validate a single Cache-Control directive against the response-directive
/// set defined in RFC 7234 §5.2.2 plus the `immutable` token added by RFC
/// 8246. Directives may take a `=token` argument (e.g. `max-age=3600`,
/// `s-maxage=120`); we accept the directive name regardless of its argument.
pub(crate) fn validate_cache_control_directive(directive: &str) -> Result<()> {
    const VALID_DIRECTIVES: &[&str] = &[
        "must-revalidate",
        "no-cache",
        "no-store",
        "no-transform",
        "public",
        "private",
        "proxy-revalidate",
        "max-age",
        "s-maxage",
        "stale-while-revalidate",
        "stale-if-error",
        "immutable",
    ];
    let trimmed = directive.trim();
    if trimmed.is_empty() {
        anyhow::bail!("blobs: cache_control entry is empty");
    }
    let name = trimmed.split('=').next().unwrap_or(trimmed).trim();
    if !VALID_DIRECTIVES
        .iter()
        .any(|d| d.eq_ignore_ascii_case(name))
    {
        anyhow::bail!(
            "blobs: invalid Cache-Control directive '{}'. Valid directives are: {}",
            name,
            VALID_DIRECTIVES.join(", ")
        );
    }
    Ok(())
}

pub(crate) fn build_put_options(
    config: &BlobConfig,
    filename: &str,
    ctx: &Context,
) -> Result<PutOptions> {
    use object_store::Attribute;

    let mut attrs = object_store::Attributes::new();

    // Cache-Control: join array with ", ".
    // Each directive is validated against the RFC-7234 §5.2 response-directive
    // set so a typo (e.g. `max_age` instead of `max-age`) surfaces here rather
    // than as a silent CDN miss in production.
    if let Some(ref cc) = config.cache_control
        && !cc.is_empty()
    {
        for directive in cc {
            validate_cache_control_directive(directive)?;
        }
        attrs.insert(Attribute::CacheControl, cc.join(", ").into());
    }

    // Content-Disposition: force-default when unset.
    //
    // The default sets
    //     ContentDisposition = "attachment;filename={{.Filename}}"
    // unconditionally when the user did not configure one, and treats `"-"`
    // as the disable-sentinel, so a copy-pasted
    // config with no `content_disposition:` key produces a downloadable blob
    // (RFC 6266 attachment) instead of an in-browser preview that the default
    // user would have seen pinning a checksum file or ZIP archive.
    //
    // Migration note: anodizer historically left this header unset by
    // default. Users relying on the old behaviour for in-browser preview
    // can opt out via `content_disposition: "-"` (sentinel kept verbatim).
    const GR_DEFAULT_CONTENT_DISPOSITION: &str = "attachment;filename={{ Filename }}";
    let resolved_disposition: Option<&str> = match config.content_disposition.as_deref() {
        // Explicit disable sentinel — emit no header.
        Some("-") => None,
        // User-supplied non-empty template — use as-is.
        Some(s) if !s.is_empty() => Some(s),
        // Unset or empty — force the default.
        _ => Some(GR_DEFAULT_CONTENT_DISPOSITION),
    };
    if let Some(disp_template) = resolved_disposition {
        // Render the template with the Filename variable added.
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
// Extra files resolution — with template-rendered names
// ---------------------------------------------------------------------------

pub(crate) fn resolve_extra_files(
    extra_files: &[ExtraFileSpec],
    ctx: &Context,
    log: &anodizer_core::log::StageLogger,
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
pub(crate) fn collect_artifacts<'a>(
    ctx: &'a Context,
    config: &BlobConfig,
    crate_name: &str,
) -> Vec<&'a Artifact> {
    if config.extra_files_only.unwrap_or(false) {
        return vec![];
    }

    // blob upload uses the canonical release-uploadable set — see
    // `release_uploadable_kinds()` in `crates/core/src/artifact.rs` for the
    // authoritative list. When `include_meta` is true, append Metadata.
    let mut uploadable_kinds: Vec<ArtifactKind> = release_uploadable_kinds().to_vec();
    if config.include_meta.unwrap_or(false) {
        uploadable_kinds.push(ArtifactKind::Metadata);
    }

    ctx.artifacts
        .all()
        .iter()
        .filter(|a| a.crate_name == crate_name)
        .filter(|a| uploadable_kinds.contains(&a.kind))
        .filter(|a| !anodizer_core::artifact::is_binary_sign_output(a))
        .filter(|a| anodizer_core::artifact::matches_id_filter(a, config.ids.as_deref()))
        .collect()
}

// ---------------------------------------------------------------------------
// Upload execution
// ---------------------------------------------------------------------------

/// Upload a per-config batch of files with intra-config parallelism via
/// tokio, given fully owned data. `BlobStage::run`'s parallel-upload phase
/// calls this on worker threads so `ctx` is never touched once the serial
/// prep phase has completed. `runtime` is shared across every blob job so a
/// single tokio thread pool serves all uploads instead of one per job.
///
/// Returns the list of fully-qualified object keys that successfully
/// landed in the store. On failure the `Err` payload carries the keys
/// that succeeded BEFORE the first failure so `BlobPublisher` can record
/// only landed uploads in `PublishEvidence::artifact_paths` — the prior
/// pre-upload capture produced a rollback checklist that referenced
/// files which were never uploaded.
// Eight params is over clippy's default of 7 — the caller fan-out lives
// inside an async block + tokio task graph, where bundling args into a
// struct adds clone/lifetime noise without simplifying the call shape.
#[allow(clippy::too_many_arguments)]
/// Result of a per-config blob upload batch.
///
/// `uploaded` holds object keys this run actually PUT (fresh or overwritten) —
/// these become rollback targets. `skipped_identical` holds keys that were
/// already present in the store with byte-identical content, so the PUT was a
/// no-op (idempotent re-run). Skipped keys are deliberately NOT rollback
/// targets: the object predates this run, and deleting it on rollback would
/// destroy state this run never created.
#[derive(Debug, Default)]
pub(crate) struct UploadReport {
    pub uploaded: Vec<String>,
    pub skipped_identical: Vec<String>,
}

/// Probe whether `object_path` already holds byte-identical content to
/// `upload_data`, so an idempotent re-run can skip the PUT.
///
/// Tri-state, mirroring cargo's `is_already_published`:
/// - `Some(true)`  → present AND identical → skip the upload.
/// - `Some(false)` → present but differs in size/bytes → overwrite (PUT).
/// - `None`        → absent, or existence/content couldn't be proven (head /
///   get error) → upload normally so a real conflict is never masked.
///
/// A size mismatch short-circuits before the (potentially large) GET; only an
/// equal-size existing object is downloaded for the byte comparison. KMS-
/// encrypted payloads are compared post-encryption — a non-deterministic
/// ciphertext simply never matches and falls through to overwrite, which is
/// safe.
async fn object_is_identical(
    store: &Arc<dyn ObjectStore>,
    object_path: &ObjectPath,
    upload_data: &[u8],
) -> Option<bool> {
    let meta = match store.head(object_path).await {
        Ok(m) => m,
        // NotFound → absent; any other head error → can't prove, so upload.
        Err(_) => return None,
    };
    if meta.size as usize != upload_data.len() {
        return Some(false);
    }
    let existing = match store.get(object_path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(_) => return None,
        },
        Err(_) => return None,
    };
    Some(existing.as_ref() == upload_data)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_files_owned(
    runtime: &tokio::runtime::Runtime,
    store: Arc<dyn ObjectStore>,
    items: Vec<(PathBuf, String)>,
    directory: String,
    put_opts_per_item: Vec<PutOptions>,
    parallelism: usize,
    client_kms: Option<(String, KmsProvider)>,
    log: &anodizer_core::log::StageLogger,
) -> Result<UploadReport> {
    runtime.block_on(async move {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism.max(1)));
        let uploaded: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let skipped: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
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
            let uploaded = Arc::clone(&uploaded);
            let skipped = Arc::clone(&skipped);
            let path_display = local_path.display().to_string();
            let local = local_path;
            let key_display = object_key.clone();
            let client_kms = client_kms.clone();
            let task_log = log.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("semaphore error: {}", e))?;
                let data = tokio::fs::read(&local).await.map_err(|e| {
                    anyhow::anyhow!("blobs: read file for upload: {}: {}", path_display, e)
                })?;

                // Capture before the Option is consumed by the if-let below.
                let kms_in_use = client_kms.is_some();
                let upload_data = if let Some((kms_key, provider)) = client_kms {
                    // Dedicated clone moved into the blocking task; `task_log`
                    // is still needed by the status/lock-recover calls below.
                    let kms_log = task_log.clone();
                    tokio::task::spawn_blocking(move || {
                        encrypt_with_kms(&data, &kms_key, provider, &kms_log)
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("KMS encryption task panicked: {}", e))??
                } else {
                    data
                };

                // Idempotency gate: when the store already holds a
                // byte-identical object at this key, the PUT is a no-op —
                // record a skip instead of blindly overwriting. A differing
                // (or unprovable) object falls through to the overwrite PUT,
                // preserving the historical blind-overwrite semantics.
                //
                // KMS-encrypted uploads skip this check: each encryption call
                // produces a different ciphertext for the same plaintext
                // (non-deterministic), so the byte comparison can never match
                // and would waste a full-object GET on every re-run.
                if !kms_in_use
                    && let Some(true) =
                        object_is_identical(&store, &object_path, &upload_data).await
                {
                    // Per-file skip detail is verbose-only; the job summary
                    // (default verbosity) reports the aggregate skip count.
                    task_log.verbose(&format!(
                        "skipped {} — identical object already present",
                        key_display
                    ));
                    anodizer_core::parallel::lock_recover(&skipped, &task_log, "blob upload")
                        .push(object_key);
                    return Ok::<(), anyhow::Error>(());
                }

                store
                    .put_opts(&object_path, upload_data.into(), put_opts)
                    .await
                    .map_err(|e| handle_upload_error(e, &path_display, &key_display))?;
                // Record the successful upload's key. Lock held only for
                // the push, so contention is negligible. Use the poison-
                // recovering helper so one panicked sibling task doesn't
                // forfeit every other worker's recorded upload — partial
                // success must still land in PublishEvidence for rollback.
                anodizer_core::parallel::lock_recover(&uploaded, &task_log, "blob upload")
                    .push(object_key);
                Ok::<(), anyhow::Error>(())
            }));
        }

        let mut first_err: Option<anyhow::Error> = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(anyhow::anyhow!("upload task panicked: {}", e));
                    }
                }
            }
        }
        let mut uploaded_keys =
            anodizer_core::parallel::lock_recover(&uploaded, log, "blob upload").clone();
        let mut skipped_keys =
            anodizer_core::parallel::lock_recover(&skipped, log, "blob upload").clone();
        // Deterministic order so evidence is reproducible across runs.
        uploaded_keys.sort();
        skipped_keys.sort();
        match first_err {
            Some(e) => Err(e),
            None => Ok(UploadReport {
                uploaded: uploaded_keys,
                skipped_identical: skipped_keys,
            }),
        }
    })
}

/// Build the `scheme://bucket[/dir]` destination prefix (no trailing slash,
/// no object key) for a job's summary line. `scheme` is the provider's
/// display name (`s3` / `gs` / `azblob`) — the same token
/// [`crate::publisher::blob_target_url`] uses for evidence URLs — so the
/// summary destination matches what rollback evidence records.
pub(crate) fn format_remote_prefix(scheme: &str, bucket: &str, directory: &str) -> String {
    let dir_trimmed = directory.trim_matches('/');
    if dir_trimmed.is_empty() {
        format!("{scheme}://{bucket}")
    } else {
        format!("{scheme}://{bucket}/{dir_trimmed}")
    }
}

pub(crate) fn format_remote_path(
    provider: Provider,
    bucket: &str,
    directory: &str,
    key: &str,
) -> String {
    let dir_trimmed = directory.trim_matches('/');
    let scheme = provider.display_name();
    if dir_trimmed.is_empty() {
        format!("{}://{}/{}", scheme, bucket, key)
    } else {
        format!("{}://{}/{}/{}", scheme, bucket, dir_trimmed, key)
    }
}

/// Format the single default-verbosity summary line for one blob upload job,
/// collapsing the per-file `uploading …` / `skipping …` firehose into one
/// line. `destination` is the `provider://bucket/dir` prefix the objects
/// landed under. Skips are objects already present byte-identical (no PUT
/// issued); uploads are objects this run actually wrote.
pub(crate) fn blob_upload_summary(uploaded: usize, skipped: usize, destination: &str) -> String {
    format!("uploaded {uploaded} object(s), skipped {skipped} (identical) → {destination}")
}

pub(crate) fn handle_upload_error(
    err: object_store::Error,
    local_path: &str,
    remote_key: &str,
) -> anyhow::Error {
    match &err {
        object_store::Error::NotFound { path, .. } => {
            anyhow::anyhow!(
                "blobs: bucket or object not found ({}): uploading {} → {}",
                path,
                local_path,
                remote_key
            )
        }
        object_store::Error::Unauthenticated { path, .. } => {
            anyhow::anyhow!(
                "blobs: authentication failed — check credentials. Uploading {} → {} ({})",
                local_path,
                remote_key,
                path
            )
        }
        object_store::Error::PermissionDenied { path, .. } => {
            anyhow::anyhow!(
                "blobs: access denied — check permissions. Uploading {} → {} ({})",
                local_path,
                remote_key,
                path
            )
        }
        _ => {
            anyhow::anyhow!(
                "blobs: upload failed for {} → {}: {}",
                local_path,
                remote_key,
                err
            )
        }
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    /// The job summary reports the upload count and the identical-skip count
    /// taken straight from the `UploadReport` vec lengths, so a job that PUT
    /// 5 objects and skipped 2 identical ones renders `uploaded 5 …, skipped 2`.
    #[test]
    fn summary_reflects_uploaded_and_skipped_counts() {
        let report = UploadReport {
            uploaded: (0..5).map(|i| format!("dir/obj{i}")).collect(),
            skipped_identical: (0..2).map(|i| format!("dir/old{i}")).collect(),
        };
        let line = blob_upload_summary(
            report.uploaded.len(),
            report.skipped_identical.len(),
            "s3://my-bucket/demo/v1.0.0",
        );
        assert_eq!(
            line,
            "uploaded 5 object(s), skipped 2 (identical) → s3://my-bucket/demo/v1.0.0"
        );
    }

    /// An all-idempotent re-run (nothing new PUT) still renders a factual
    /// summary with a zero upload count rather than suppressing the line.
    #[test]
    fn summary_handles_zero_uploads() {
        let line = blob_upload_summary(0, 3, "gs://bucket/demo/v2");
        assert_eq!(
            line,
            "uploaded 0 object(s), skipped 3 (identical) → gs://bucket/demo/v2"
        );
    }
}
