use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind, release_uploadable_kinds};
use anodizer_core::config::{BlobConfig, ExtraFileSpec};
use anodizer_core::context::Context;
use anodizer_core::extrafiles;
use anodizer_core::template;

use object_store::buffered::BufWriter;
use object_store::path::Path as ObjectPath;
use object_store::{
    Attribute, Attributes, GetOptions, ObjectMeta, ObjectStore, ObjectStoreExt, PutOptions,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::content_type::{SNIFF_BYTES, detect_content_type};
use crate::kms::{KmsProvider, encrypt_with_kms};
use crate::provider::Provider;

// ---------------------------------------------------------------------------
// Put options — headers (cache-control, content-disposition)
// ---------------------------------------------------------------------------

/// Validate a single Cache-Control directive against the response-directive
/// set defined in RFC 7234 §5.2.2 plus the `immutable` token added by RFC
/// 8246. Directives may take a `=token` argument (e.g. `max-age=3600`,
/// `s-maxage=120`); the directive name is accepted regardless of its argument.
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
    const DEFAULT_CONTENT_DISPOSITION: &str = "attachment;filename={{ Filename }}";
    let resolved_disposition: Option<&str> = match config.content_disposition.as_deref() {
        // Explicit disable sentinel — emit no header.
        Some("-") => None,
        // User-supplied non-empty template — use as-is.
        Some(s) if !s.is_empty() => Some(s),
        // Unset or empty — force the default.
        _ => Some(DEFAULT_CONTENT_DISPOSITION),
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
///
/// `log` carries the exclude-eliminated-all warning: when a non-empty
/// `exclude:` reduces a non-empty candidate set to zero, a typo'd glob (e.g.
/// `*` instead of `*.sig`) would otherwise silently drop every asset.
pub(crate) fn collect_artifacts<'a>(
    ctx: &'a Context,
    config: &BlobConfig,
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
) -> Vec<&'a Artifact> {
    if config.extra_files_only.unwrap_or(false) {
        return vec![];
    }

    // blob upload uses the canonical release-uploadable set — see
    // `release_uploadable_kinds()` in `crates/core/src/artifact/kind.rs` for the
    // authoritative list. When `include_meta` is true, append Metadata.
    let mut uploadable_kinds: Vec<ArtifactKind> = release_uploadable_kinds().to_vec();
    if config.include_meta.unwrap_or(false) {
        uploadable_kinds.push(ArtifactKind::Metadata);
    }

    let id_filtered: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.crate_name == crate_name)
        .filter(|a| uploadable_kinds.contains(&a.kind))
        .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
        .filter(|a| anodizer_core::artifact::matches_id_filter(a, config.ids.as_deref()))
        .collect();
    let pre_exclude = id_filtered.len();
    let kept: Vec<&Artifact> = id_filtered
        .into_iter()
        .filter(|a| anodizer_core::artifact::passes_exclude_filter(a, config.exclude.as_deref()))
        .collect();
    if anodizer_core::artifact::exclude_filter_eliminated_all(
        config.exclude.as_deref(),
        pre_exclude,
        kept.len(),
    ) {
        log.warn(&format!(
            "exclude filter {:?} dropped all {} candidate artifact(s) for blob config on \
             crate '{}'; check the globs match asset names, not full paths",
            config.exclude.as_deref().unwrap_or_default(),
            pre_exclude,
            crate_name
        ));
    }
    kept
}

// ---------------------------------------------------------------------------
// Upload execution
// ---------------------------------------------------------------------------

/// Result of a per-config blob upload batch.
///
/// `uploaded` holds object keys this run actually PUT (fresh or overwritten) —
/// these become rollback targets. `skipped_identical` holds keys that were
/// already present in the store with byte-identical content, so the PUT was a
/// no-op (idempotent re-run). Skipped keys are deliberately NOT rollback
/// targets: the object predates this run, and deleting it on rollback would
/// destroy state this run never created.
///
/// `overwrote` is the subset of `uploaded` whose key already held an object
/// before this run's PUT (or whose existence could not be ruled out). A
/// rollback deletes an object this run created; one it overwrote has a
/// previous version nothing here can restore, so it is left in place.
#[derive(Debug, Default)]
pub(crate) struct UploadReport {
    pub uploaded: Vec<String>,
    pub skipped_identical: Vec<String>,
    pub overwrote: Vec<String>,
}

/// Size of one part of a streamed upload, and the size from which a file is
/// sent as a multipart upload instead of a single PUT. Above every provider's
/// minimum part size (5 MiB on S3 and GCS), and large enough that S3's
/// 10,000-part limit is reached only past 97 GiB.
pub(crate) const UPLOAD_PART_BYTES: usize = 10 * 1024 * 1024;

/// Parts of one file in flight at once.
///
/// One upload holds at most this many parts in flight plus the part being
/// filled, so its memory is bounded at
/// `(UPLOAD_PART_CONCURRENCY + 1) * UPLOAD_PART_BYTES` plus the
/// [`READ_CHUNK_BYTES`] read buffer — 51 MiB — whatever the file's size. A
/// config uploading several files at once holds that per file in flight, so
/// the bound for a run is that figure times the config's `parallelism`.
pub(crate) const UPLOAD_PART_CONCURRENCY: usize = 4;

/// Size of the buffer a file is read through on its way to the store.
const READ_CHUNK_BYTES: usize = 1024 * 1024;

/// How long the abort of a failed multipart upload may take by default. The abort
/// follows a failure, often of the same server, and the store client retries
/// an unanswered request for minutes on its own.
pub(crate) const ABORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What the store holds at a key before this run writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingObject {
    /// `head` answered not-found: the PUT creates the key.
    Absent,
    /// Present with byte-identical content: the PUT is skipped.
    Identical,
    /// Present with different size or bytes: the PUT overwrites it.
    Differs,
    /// Present, or unanswerable (head / get / local read error): the PUT
    /// goes ahead, and the key is treated as overwritten.
    Unknown,
}

/// Probe what `object_path` already holds compared with the file at `local`,
/// so an idempotent re-run can skip the PUT and a rollback knows whether the
/// key existed before the run. Mirrors cargo's `is_already_published`: only
/// a proven identical object skips the upload, so a real conflict is never
/// masked.
///
/// A size mismatch short-circuits before any download; an equal-size object
/// is compared against the file one range at a time, so neither side is held
/// in memory whole.
async fn probe_existing_object(
    store: &Arc<dyn ObjectStore>,
    object_path: &ObjectPath,
    local: &std::path::Path,
) -> ExistingObject {
    let meta = match store.head(object_path).await {
        Ok(meta) => meta,
        Err(object_store::Error::NotFound { .. }) => return ExistingObject::Absent,
        Err(_) => return ExistingObject::Unknown,
    };
    let Ok(file) = tokio::fs::File::open(local).await else {
        return ExistingObject::Unknown;
    };
    match file.metadata().await {
        Ok(local_meta) if local_meta.len() != meta.size => return ExistingObject::Differs,
        Ok(_) => {}
        Err(_) => return ExistingObject::Unknown,
    }
    match object_matches_reader(store, &meta, file, UPLOAD_PART_BYTES).await {
        Some(true) => ExistingObject::Identical,
        Some(false) => ExistingObject::Differs,
        None => ExistingObject::Unknown,
    }
}

/// Compare the object `meta` describes with everything `reader` yields,
/// fetching the object `range_bytes` at a time.
///
/// Identical means every byte of the object was compared and the reader ended
/// exactly there: a reader that ends early or runs longer differs. Each range
/// is requested for the version `meta` names, so an object replaced between
/// two ranges is reported as differing instead of being compared as a mix of
/// two versions.
pub(crate) async fn object_matches_reader<R: AsyncRead + Unpin>(
    store: &Arc<dyn ObjectStore>,
    meta: &ObjectMeta,
    mut reader: R,
    range_bytes: usize,
) -> Option<bool> {
    let mut ours = vec![0u8; READ_CHUNK_BYTES.min(range_bytes.max(1))];
    let mut compared = 0u64;
    while compared < meta.size {
        let end = meta.size.min(compared + range_bytes.max(1) as u64);
        let options = GetOptions {
            if_match: meta.e_tag.clone(),
            version: meta.version.clone(),
            range: Some((compared..end).into()),
            ..Default::default()
        };
        let theirs = match store.get_opts(&meta.location, options).await {
            Ok(result) => result.bytes().await.ok()?,
            Err(object_store::Error::Precondition { .. }) => return Some(false),
            Err(_) => return None,
        };
        if theirs.len() as u64 != end - compared {
            return None;
        }
        let mut at = 0;
        while at < theirs.len() {
            let want = ours.len().min(theirs.len() - at);
            let n = reader.read(&mut ours[..want]).await.ok()?;
            if n == 0 || ours[..n] != theirs[at..at + n] {
                return Some(false);
            }
            at += n;
        }
        compared = end;
    }
    Some(reader.read(&mut ours[..1]).await.ok()? == 0)
}

/// Where one file is streamed to, and in what shape: a file smaller than
/// `part_bytes` is sent as one PUT, a larger one as a multipart upload of
/// parts that size with `concurrency` of them in flight, and `attributes` are
/// set on the object either way.
pub(crate) struct UploadSink {
    pub store: Arc<dyn ObjectStore>,
    pub destination: UploadDestination,
    pub object_path: ObjectPath,
    pub attributes: Attributes,
    pub part_bytes: usize,
    pub concurrency: usize,
    /// How long the abort of a failed multipart upload may take.
    pub abort_timeout: std::time::Duration,
}

impl UploadSink {
    /// The sink every published file goes through: [`UPLOAD_PART_BYTES`]
    /// parts, [`UPLOAD_PART_CONCURRENCY`] in flight.
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        destination: UploadDestination,
        object_path: ObjectPath,
        attributes: Attributes,
    ) -> Self {
        Self {
            store,
            destination,
            object_path,
            attributes,
            part_bytes: UPLOAD_PART_BYTES,
            concurrency: UPLOAD_PART_CONCURRENCY,
            abort_timeout: ABORT_TIMEOUT,
        }
    }
}

/// Copy `reader` into `sink` through a fixed-size buffer and commit the
/// object once the reader is exhausted.
///
/// The object's `Content-Type` is decided from the reader's first
/// [`SNIFF_BYTES`] bytes, which are then uploaded like the rest.
///
/// A read or a write that fails part way aborts the upload instead of
/// committing what arrived so far, so no truncated object is left at the key.
/// The parts of a multipart upload are a separate matter: they are removed
/// only when the abort itself reaches the store, and a warning names the key
/// when it did not.
pub(crate) async fn stream_upload<R: AsyncRead + Unpin>(
    mut reader: R,
    sink: UploadSink,
    local_path: &str,
    remote_key: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let read_error =
        |e: std::io::Error| anyhow::anyhow!("blobs: read file for upload: {}: {}", local_path, e);
    let UploadSink {
        store,
        destination,
        object_path,
        mut attributes,
        part_bytes,
        concurrency,
        abort_timeout,
    } = sink;

    let mut buf = vec![0u8; READ_CHUNK_BYTES.max(SNIFF_BYTES)];
    let mut pending = 0;
    while pending < SNIFF_BYTES {
        match reader.read(&mut buf[pending..SNIFF_BYTES]).await {
            Ok(0) => break,
            Ok(n) => pending += n,
            Err(e) => return Err(read_error(e)),
        }
    }
    attributes.insert(
        Attribute::ContentType,
        detect_content_type(&buf[..pending]).into(),
    );
    let mut writer = BufWriter::with_capacity(store, object_path, part_bytes)
        .with_max_concurrency(concurrency)
        .with_attributes(attributes);

    // The writer opens the multipart upload inside the write that brings the
    // total to one part, so a failure of any later write, or of the shutdown
    // of an upload that got that far, comes from a part or the completion.
    let mut sent = 0usize;
    while pending > 0 {
        let multipart_open = sent >= part_bytes;
        if let Err(e) = writer.write_all(&buf[..pending]).await {
            abort_upload(
                &mut writer,
                abort_timeout,
                &destination,
                remote_key,
                multipart_open,
                log,
            )
            .await;
            return Err(handle_stream_error(
                e,
                local_path,
                remote_key,
                multipart_open,
            ));
        }
        sent += pending;
        pending = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                abort_upload(
                    &mut writer,
                    abort_timeout,
                    &destination,
                    remote_key,
                    sent >= part_bytes,
                    log,
                )
                .await;
                return Err(read_error(e));
            }
        };
    }

    let multipart_open = sent >= part_bytes;
    writer.shutdown().await.map_err(|e| {
        // A multipart shutdown that fails attempts the abort itself and does
        // not say whether it went through, and aborting the writer a second
        // time panics, so the parts cannot be known to be gone.
        let upload_gone = e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<object_store::Error>())
            .is_some_and(|inner| matches!(inner, object_store::Error::NotFound { .. }));
        if multipart_open && !upload_gone {
            log.warn(&incomplete_upload_warning(
                &destination,
                remote_key,
                "completing the upload failed",
            ));
        }
        handle_stream_error(e, local_path, remote_key, multipart_open)
    })
}

/// Abort the multipart upload behind `writer`, warning when the parts
/// already sent could not be removed. A writer still below one part holds no
/// upload and returns at once.
///
/// `multipart_open` says a part had been sent before the failure. Azure has
/// no abort: its blocks are staged by `Put Block` and nothing but a commit
/// or the service's own expiry removes them, so the store answers the abort
/// with success having removed nothing, and an open upload there is warned
/// about on that answer too.
async fn abort_upload(
    writer: &mut BufWriter,
    timeout: std::time::Duration,
    destination: &UploadDestination,
    remote_key: &str,
    multipart_open: bool,
    log: &anodizer_core::log::StageLogger,
) {
    let why = match tokio::time::timeout(timeout, writer.abort()).await {
        Ok(Ok(())) if multipart_open && matches!(destination.provider, Provider::AzBlob) => {
            "Azure has no abort that removes blocks already staged".to_string()
        }
        // The store no longer has the upload (aborted from outside, or
        // expired by a lifecycle rule), so no parts are left behind.
        Ok(Ok(())) | Ok(Err(object_store::Error::NotFound { .. })) => return,
        Ok(Err(e)) => format!("aborting it failed: {e}"),
        Err(_) => format!("aborting it got no answer within {timeout:?}"),
    };
    log.warn(&incomplete_upload_warning(destination, remote_key, &why));
}

/// The bucket a job uploads to, as far as a cleanup instruction has to name
/// it.
#[derive(Debug, Clone)]
pub(crate) struct UploadDestination {
    pub provider: Provider,
    pub bucket: String,
    /// The custom S3 endpoint, which the `aws` commands in a warning must be
    /// pointed at too.
    pub endpoint: Option<String>,
}

#[cfg(test)]
impl UploadDestination {
    pub(crate) fn test_s3() -> Self {
        Self {
            provider: Provider::S3,
            bucket: "bucket".to_string(),
            endpoint: None,
        }
    }
}

/// The warning for a multipart upload whose parts may have outlived it, with
/// the cleanup each provider offers.
///
/// The S3 commands are complete: pasted as printed, the first lists the
/// upload's ID and the second needs only that ID filled in. Every value the
/// config supplied is single-quoted for the shell, so a key holding a space,
/// a `$` or a quote is still one argument.
pub(crate) fn incomplete_upload_warning(
    destination: &UploadDestination,
    remote_key: &str,
    why: &str,
) -> String {
    use anodizer_core::shell::shell_single_quote;
    let UploadDestination {
        provider,
        bucket,
        endpoint,
    } = destination;
    let at = format!("{}://{bucket}/{remote_key}", provider.display_name());
    match provider {
        Provider::S3 => {
            let key = shell_single_quote(remote_key);
            let bucket = shell_single_quote(bucket);
            let endpoint = endpoint
                .as_deref()
                .filter(|e| !e.is_empty())
                .map(|e| format!(" --endpoint-url {}", shell_single_quote(e)))
                .unwrap_or_default();
            format!(
                "an incomplete multipart upload may remain at {at} ({why}); its parts are \
                 stored and billed until it is aborted — find its upload ID with `aws s3api \
                 list-multipart-uploads --bucket {bucket} --prefix {key}{endpoint}` and remove \
                 it with `aws s3api abort-multipart-upload --bucket {bucket} --key {key} \
                 --upload-id <UploadId>{endpoint}`, or set an AbortIncompleteMultipartUpload \
                 lifecycle rule on the bucket"
            )
        }
        Provider::Gcs => format!(
            "an incomplete multipart upload may remain at {at} ({why}); its parts are stored \
             and billed until it is aborted — gcloud has no command that lists or aborts one, \
             so set an AbortIncompleteMultipartUpload lifecycle rule with an `age` condition \
             on the bucket, or abort it through the XML API"
        ),
        Provider::AzBlob => format!(
            "uncommitted blocks may remain at {at} ({why}); Azure removes them a week after \
             the last block was written, and uploading the file again removes them sooner"
        ),
    }
}

/// [`handle_upload_error`] for a failure the streaming writer reported: the
/// writer wraps the store's own error in an `io::Error`.
///
/// `multipart_open` says the multipart upload had been opened before the
/// failing call, which is when a `NotFound` is about that upload and no
/// longer about the bucket.
fn handle_stream_error(
    err: std::io::Error,
    local_path: &str,
    remote_key: &str,
    multipart_open: bool,
) -> anyhow::Error {
    match err.downcast::<object_store::Error>() {
        Ok(object_store::Error::NotFound { path, .. }) if multipart_open => anyhow::anyhow!(
            "blobs: the multipart upload was aborted or expired part way through ({}): \
             uploading {} → {}; run the release again to upload the file",
            path,
            local_path,
            remote_key
        ),
        Ok(store_err) => handle_upload_error(store_err, local_path, remote_key),
        Err(other) => anyhow::anyhow!(
            "blobs: upload failed for {} → {}: {}",
            local_path,
            remote_key,
            other
        ),
    }
}

/// Upload a per-config batch of files with intra-config parallelism via
/// tokio, given fully owned inputs. `BlobStage::run`'s parallel-upload phase
/// calls this on worker threads so `ctx` is never touched once the serial
/// prep phase has completed. `runtime` is shared across every blob job so a
/// single tokio thread pool serves all uploads instead of one per job.
///
/// Each file is streamed from disk through [`stream_upload`], so memory held
/// per upload is bounded by the part size, not by the file's size. Only a
/// client-side KMS upload reads its file whole.
///
/// Returns the object keys that ended up in the store, split into those this
/// run wrote and those already present byte-identical, together with the
/// first failure when a file did not upload. The report is complete either
/// way: the keys written before and beside a failed file are in it, because
/// those objects exist and a rollback has to remove them.
// Nine params is over clippy's default of 7 — the caller fan-out lives
// inside an async block + tokio task graph, where bundling args into a
// struct adds clone/lifetime noise without simplifying the call shape.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_files_owned(
    runtime: &tokio::runtime::Runtime,
    store: Arc<dyn ObjectStore>,
    items: Vec<(PathBuf, String)>,
    directory: String,
    put_opts_per_item: Vec<PutOptions>,
    parallelism: usize,
    client_kms: Option<(String, KmsProvider)>,
    destination: UploadDestination,
    log: &anodizer_core::log::StageLogger,
) -> (UploadReport, Result<()>) {
    // Read on the caller's thread: each upload task is polled by a runtime
    // worker that holds no scope of its own.
    let retry_scope = anodizer_core::retry::current_scope();
    runtime.block_on(async move {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism.max(1)));
        let uploaded: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let skipped: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let overwrote: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for ((local_path, remote_key), mut put_opts) in items.into_iter().zip(put_opts_per_item) {
            let dir_trimmed = directory.trim_matches('/');
            let object_key = if dir_trimmed.is_empty() {
                remote_key.clone()
            } else {
                format!("{}/{}", dir_trimmed, remote_key)
            };

            let object_path = ObjectPath::from(object_key.as_str());

            let store = Arc::clone(&store);
            let destination = destination.clone();
            let sem = Arc::clone(&semaphore);
            let uploaded = Arc::clone(&uploaded);
            let skipped = Arc::clone(&skipped);
            let overwrote = Arc::clone(&overwrote);
            let path_display = local_path.display().to_string();
            let local = local_path;
            let key_display = object_key.clone();
            let client_kms = client_kms.clone();
            let task_log = log.clone();
            let task_retry_scope = retry_scope.clone();

            handles.push(tokio::spawn(anodizer_core::retry::in_scope(
                task_retry_scope,
                async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore error: {}", e))?;
                    // Client-side encryption takes the whole plaintext in one
                    // call, so this is the one path that reads a file whole. The
                    // size each provider accepts is checked before any upload
                    // starts (`validate_kms_plaintext_size`), 64 KiB at most.
                    //
                    // It also skips the idempotency gate below: each encryption
                    // call produces a different ciphertext for the same
                    // plaintext, so the comparison can never match.
                    if let Some((kms_key, provider)) = client_kms {
                        // Nothing compares the ciphertext, so the only
                        // question is whether the key exists.
                        let existed = !matches!(
                            store.head(&object_path).await,
                            Err(object_store::Error::NotFound { .. })
                        );
                        let data = tokio::fs::read(&local).await.map_err(|e| {
                            anyhow::anyhow!("blobs: read file for upload: {}: {}", path_display, e)
                        })?;
                        // Dedicated clone moved into the blocking task; `task_log`
                        // is still needed by the lock-recover call below.
                        let kms_log = task_log.clone();
                        let encrypted = tokio::task::spawn_blocking(move || {
                            encrypt_with_kms(&data, &kms_key, provider, &kms_log)
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("KMS encryption task panicked: {}", e))??;
                        put_opts.attributes.insert(
                            Attribute::ContentType,
                            detect_content_type(&encrypted).into(),
                        );
                        store
                            .put_opts(&object_path, encrypted.into(), put_opts)
                            .await
                            .map_err(|e| handle_upload_error(e, &path_display, &key_display))?;
                        if existed {
                            anodizer_core::parallel::lock_recover(
                                &overwrote,
                                &task_log,
                                "blob upload",
                            )
                            .push(object_key.clone());
                        }
                        anodizer_core::parallel::lock_recover(&uploaded, &task_log, "blob upload")
                            .push(object_key);
                        return Ok::<(), anyhow::Error>(());
                    }

                    // Idempotency gate: when the store already holds a
                    // byte-identical object at this key, the PUT is a no-op —
                    // record a skip instead of blindly overwriting. A differing
                    // (or unprovable) object falls through to the overwrite PUT,
                    // preserving the historical blind-overwrite semantics.
                    let existing = probe_existing_object(&store, &object_path, &local).await;
                    if existing == ExistingObject::Identical {
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

                    // Opened here, per attempt, so a re-run after a failure
                    // reads the file from its first byte again.
                    let file = tokio::fs::File::open(&local).await.map_err(|e| {
                        anyhow::anyhow!("blobs: read file for upload: {}: {}", path_display, e)
                    })?;
                    stream_upload(
                        file,
                        UploadSink::new(
                            Arc::clone(&store),
                            destination,
                            object_path,
                            put_opts.attributes,
                        ),
                        &path_display,
                        &key_display,
                        &task_log,
                    )
                    .await?;
                    // Record the successful upload's key. Lock held only for
                    // the push, so contention is negligible. Use the poison-
                    // recovering helper so one panicked sibling task doesn't
                    // forfeit every other worker's recorded upload — partial
                    // success must still end up in PublishEvidence for rollback.
                    if existing != ExistingObject::Absent {
                        anodizer_core::parallel::lock_recover(&overwrote, &task_log, "blob upload")
                            .push(object_key.clone());
                    }
                    anodizer_core::parallel::lock_recover(&uploaded, &task_log, "blob upload")
                        .push(object_key);
                    Ok::<(), anyhow::Error>(())
                },
            )));
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
        let mut overwrote_keys =
            anodizer_core::parallel::lock_recover(&overwrote, log, "blob upload").clone();
        // Deterministic order so evidence is reproducible across runs.
        uploaded_keys.sort();
        skipped_keys.sort();
        overwrote_keys.sort();
        (
            UploadReport {
                uploaded: uploaded_keys,
                skipped_identical: skipped_keys,
                overwrote: overwrote_keys,
            },
            first_err.map_or(Ok(()), Err),
        )
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
/// ended up under. Skips are objects already present byte-identical (no PUT
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
            overwrote: Vec::new(),
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

#[cfg(test)]
mod collect_artifacts_tests {
    use super::*;
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    /// A macOS `.app` bundle is registered as `ArtifactKind::Installer` (part of
    /// the release-uploadable set) but lives on disk as a DIRECTORY. The blob
    /// uploader later reads each selected path as a file; a directory dies
    /// EISDIR. The bundle must be dropped from `collect_artifacts` while a
    /// sibling installer FILE is still selected.
    #[test]
    fn collect_artifacts_skips_appbundle_directory() {
        let config = Config {
            project_name: "anodizer".to_string(),
            crates: vec![CrateConfig {
                name: "anodizer".to_string(),
                path: ".".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: "anodizer_amd64.app".to_string(),
            path: PathBuf::from("/dist/anodizer_amd64.app"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "anodizer".to_string(),
            metadata: std::collections::HashMap::from([(
                "format".to_string(),
                "appbundle".to_string(),
            )]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: "anodizer_amd64.msi".to_string(),
            path: PathBuf::from("/dist/anodizer_amd64.msi"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "anodizer".to_string(),
            metadata: std::collections::HashMap::from([("format".to_string(), "msi".to_string())]),
            size: None,
        });

        let blob = BlobConfig {
            provider: "s3".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        };
        let log =
            anodizer_core::log::StageLogger::new("blob-test", anodizer_core::log::Verbosity::Quiet);
        let selected = collect_artifacts(&ctx, &blob, "anodizer", &log);
        let names: Vec<&str> = selected.iter().map(|a| a.name.as_str()).collect();

        assert!(
            names.contains(&"anodizer_amd64.msi"),
            "the installer FILE must be selected for upload; got {names:?}"
        );
        assert!(
            !names.contains(&"anodizer_amd64.app"),
            "the .app DIRECTORY must be skipped, not uploaded; got {names:?}"
        );
    }
}
