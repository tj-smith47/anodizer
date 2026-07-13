//! PyPI legacy (twine-protocol) upload: one multipart `POST` per file
//! against the repository URL, authenticated as `__token__` via HTTP Basic
//! auth.
//!
//! Duplicate handling: PyPI answers a re-upload of an existing filename
//! with `400 File already exists` (Warehouse), other indexes use `409` or a
//! `403` re-upload message. With `skip_existing` the publisher treats those
//! shapes as an idempotent skip (with a status line), matching twine's
//! `--skip-existing`.

use std::ops::ControlFlow;
use std::path::Path;

use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, retry_sync_deadline, status_is_retriable};
use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

use super::wheel::WheelSpec;

/// Default upload endpoint — production PyPI's legacy upload API.
pub(crate) const DEFAULT_REPOSITORY: &str = "https://upload.pypi.org/legacy/";

/// What kind of distribution one upload carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileType {
    Wheel,
    Sdist,
}

impl FileType {
    /// The upload API's `filetype` field value.
    pub(crate) fn filetype(self) -> &'static str {
        match self {
            Self::Wheel => "bdist_wheel",
            Self::Sdist => "sdist",
        }
    }

    /// The upload API's `pyversion` field value.
    pub(crate) fn pyversion(self) -> &'static str {
        match self {
            Self::Wheel => "py3",
            Self::Sdist => "source",
        }
    }
}

/// Terminal state of one file's upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UploadOutcome {
    /// Bytes landed this run.
    Uploaded { sha256: String },
    /// The index already holds this filename and `skip_existing` folded the
    /// rejection into an idempotent skip.
    SkippedExisting { sha256: String },
}

/// True when an upload rejection is the index's "file already exists"
/// shape: Warehouse's `400` (body names the conflict), a generic `409`,
/// or a `403` re-upload refusal. Matched generously — a false negative
/// fails a legitimately-idempotent re-run, while a false positive only
/// skips a file the index refused anyway.
pub(crate) fn is_duplicate_rejection(status: u16, body: &str) -> bool {
    if status == 409 {
        return true;
    }
    if status == 400 || status == 403 {
        let lower = body.to_ascii_lowercase();
        // "this filename has already been used" is Warehouse's rejection for a
        // name whose FILE was deleted but whose slot is still burned (the
        // one-way door); twine matches the same phrase. Without it an
        // idempotent re-run after a file deletion hard-fails under
        // skip_existing.
        return lower.contains("already exist")
            || lower.contains("file already")
            || lower.contains("filename has already been used");
    }
    false
}

/// Upload one file. `normalized_name` is the PEP 503 form the index keys
/// on; the METADATA-derived fields ride along per the legacy API contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_file(
    client: &reqwest::blocking::Client,
    repository: &str,
    token: &str,
    normalized_name: &str,
    spec: &WheelSpec,
    file_type: FileType,
    path: &Path,
    skip_existing: bool,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<UploadOutcome> {
    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Read once to digest; the upload body itself streams from the file
    // (a file-backed `Part` per attempt), so a retried upload never clones
    // the whole distribution into memory again.
    let sha256 = {
        let bytes =
            std::fs::read(path).with_context(|| format!("pypi: read '{}'", path.display()))?;
        anodizer_core::hashing::hex_lower(&Sha256::digest(&bytes))
    };

    log.verbose(&format!(
        "POST {} ({} {}, sha256 {})",
        repository,
        file_type.filetype(),
        filename,
        sha256
    ));

    // Duplicate rejections keep a transient-error retry floor even when a
    // stateful mode resolves `max_attempts` to 1; re-POSTs of the same file
    // are idempotent (the index keys on the filename).
    let upload_policy = policy.with_idempotent_floor();
    let duplicate = std::cell::Cell::new(false);
    retry_sync_deadline(
        RetryLog::new("pypi upload", log),
        &upload_policy,
        deadline,
        |_attempt| {
            let mut form = reqwest::blocking::multipart::Form::new()
                .text(":action", "file_upload")
                .text("protocol_version", "1")
                .text("name", normalized_name.to_string())
                .text("version", spec.version.clone())
                .text("filetype", file_type.filetype())
                .text("pyversion", file_type.pyversion())
                .text("metadata_version", spec.metadata_version.clone())
                .text("sha256_digest", sha256.clone());
            if let Some(s) = &spec.summary {
                form = form.text("summary", s.clone());
            }
            if let Some(a) = &spec.author {
                form = form.text("author", a.clone());
            }
            if let Some(a) = &spec.author_email {
                form = form.text("author_email", a.clone());
            }
            if let Some(l) = &spec.license {
                form = form.text("license", l.clone());
            }
            if let Some(h) = &spec.homepage {
                form = form.text("home_page", h.clone());
            }
            // Legacy-API `project_urls` field: one repeated `Label, URL` entry
            // per link, matching the METADATA `Project-URL` headers.
            for (label, url) in &spec.project_urls {
                form = form.text("project_urls", format!("{}, {}", label, url));
            }
            if let Some(r) = &spec.requires_python {
                form = form.text("requires_python", r.clone());
            }
            if !spec.keywords.is_empty() {
                form = form.text("keywords", spec.keywords.join(","));
            }
            for c in &spec.classifiers {
                form = form.text("classifiers", c.clone());
            }
            if let Some(d) = &spec.description {
                form = form.text("description", d.clone());
                if let Some(ct) = &spec.description_content_type {
                    form = form.text("description_content_type", ct.clone());
                }
            }
            let file_part = match reqwest::blocking::multipart::Part::file(path) {
                Ok(p) => p,
                Err(e) => {
                    return Err(ControlFlow::Break(
                        anyhow::Error::new(e)
                            .context(format!("pypi: open '{}' for upload", filename)),
                    ));
                }
            };
            let file_part = match file_part
                .file_name(filename.clone())
                .mime_str("application/octet-stream")
            {
                Ok(p) => p,
                Err(e) => {
                    return Err(ControlFlow::Break(
                        anyhow::Error::new(e)
                            .context(format!("pypi: build multipart part for '{}'", filename)),
                    ));
                }
            };
            form = form.part("content", file_part);

            let resp = match client
                .post(repository)
                .basic_auth("__token__", Some(token))
                .multipart(form)
                .send()
            {
                Ok(r) => r,
                Err(e) => {
                    // Transport-level failure — retry.
                    return Err(ControlFlow::Continue(
                        anyhow::Error::new(e).context(format!("pypi: send POST {}", repository)),
                    ));
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            let body = resp.text().unwrap_or_default();
            if is_duplicate_rejection(status.as_u16(), &body) {
                duplicate.set(true);
                return Ok(());
            }
            let err = anyhow::anyhow!(
                "pypi: POST {} for '{}' returned HTTP {}: {}",
                repository,
                filename,
                status,
                redact_bearer_tokens(body.trim())
            );
            if status_is_retriable(status.as_u16()) {
                Err(ControlFlow::Continue(err))
            } else {
                Err(ControlFlow::Break(err))
            }
        },
    )?;

    if duplicate.get() {
        if !skip_existing {
            anyhow::bail!(
                "pypi: '{}' already exists on {} and `skip_existing: false` makes a \
                 duplicate upload a hard error (a published filename can never be \
                 replaced — bump the version to ship new bytes)",
                filename,
                repository
            );
        }
        log.status(&format!(
            "skipped '{}' — already on {} (idempotent)",
            filename, repository
        ));
        return Ok(UploadOutcome::SkippedExisting { sha256 });
    }
    Ok(UploadOutcome::Uploaded { sha256 })
}
