use super::*;

/// Retry an HTTP request builder, threading classification through the
/// shared [`retry_http_blocking`] helper. `build_send` is called per attempt
/// so multipart bodies can be rebuilt. 5xx/429 + transport errors retry;
/// 4xx fast-fails. Returns `(status, body)` on success.
pub(crate) fn retry_request<F>(
    label: &str,
    art_name: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
    mut build_send: F,
) -> Result<(reqwest::StatusCode, String)>
where
    F: FnMut() -> Result<reqwest::blocking::Response, reqwest::Error>,
{
    let scope = format!("cloudsmith {label} for '{art_name}'");
    retry_http_blocking_deadline(
        RetryLog::new(&scope, log),
        policy,
        deadline,
        SuccessClass::Strict,
        |attempt| {
            if attempt > 1 {
                log.verbose(&format!(
                    "retrying cloudsmith {label} for '{art_name}' (attempt {attempt})"
                ));
            }
            build_send()
        },
        |status, body| {
            format!(
                "cloudsmith {label} for '{art_name}' returned HTTP {status}: {}",
                redact_bearer_tokens(body.trim())
            )
        },
    )
}

/// Stage a file for upload: request a `files/create` slot (step 1) and push
/// the bytes to the returned S3 presigned URL (step 2). Returns the
/// single-use `identifier` the caller passes to `packages/upload` (step 3).
///
/// A Cloudsmith files/create slot is consumed by exactly one package-create,
/// so a caller uploading to N distributions must call this once per
/// distribution to obtain N distinct identifiers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_cloudsmith_file(
    client: &reqwest::blocking::Client,
    api_base: &str,
    organization: &str,
    repository: &str,
    art_name: &str,
    md5_hex: &str,
    file_bytes: &[u8],
    token: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<String> {
    // --- Step 1/3: request a files/create slot ---
    //
    // POST /v1/files/{org}/{repo}/ with the filename + md5 returns a
    // short-lived S3 presigned upload URL plus the fields the upload POST
    // must include. This matches what the official Cloudsmith CLI's
    // `request_file_upload` helper does.
    let files_create_url = format!("{}/files/{}/{}/", api_base, organization, repository);
    let files_create_body = serde_json::json!({
        "filename": art_name,
        "md5_checksum": md5_hex,
        "method": "post",
    });

    log.verbose(&format!("POST {} (step 1 of 3)", files_create_url));
    let (_create_status, create_body) =
        retry_request("files/create", art_name, policy, deadline, log, || {
            client
                .post(&files_create_url)
                .header("Authorization", format!("token {}", token))
                .header("Accept", "application/json")
                .json(&files_create_body)
                .send()
        })?;
    let create_json: serde_json::Value = serde_json::from_str(&create_body).with_context(|| {
        format!(
            "cloudsmith files/create for '{}' returned non-JSON body: {}",
            art_name,
            create_body.trim()
        )
    })?;
    let identifier = create_json
        .get("identifier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudsmith files/create response missing 'identifier' for '{}': {}",
                art_name,
                create_body.trim()
            )
        })?
        .to_string();
    let presigned_url = create_json
        .get("upload_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudsmith files/create response missing 'upload_url' for '{}'",
                art_name
            )
        })?
        .to_string();
    let upload_fields = create_json
        .get("upload_fields")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // --- Step 2/3: upload bytes to the presigned S3 URL ---
    //
    // The presigned URL is AWS S3 POST form — no Cloudsmith auth header is
    // added here. The fields returned in step 1 (policy, signature, key, ...)
    // MUST be included as multipart form text parts exactly as given, and the
    // actual file goes under the `file` key (not `package_file`).
    log.verbose(&format!("POST {} (presigned, step 2 of 3)", presigned_url));
    // Multipart Form is move-only, so we rebuild it on every retry attempt.
    // Cloning `file_bytes` and `upload_fields` per-attempt is the price of
    // retriability; the bytes are already in memory.
    let _ = retry_request("presigned upload", art_name, policy, deadline, log, || {
        let mut form = reqwest::blocking::multipart::Form::new();
        for (k, v) in &upload_fields {
            let val = v
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string());
            form = form.text(k.clone(), val);
        }
        let file_part = match reqwest::blocking::multipart::Part::bytes(file_bytes.to_vec())
            .file_name(art_name.to_string())
            .mime_str("application/octet-stream")
        {
            Ok(p) => p,
            // `mime_str` only fails on unparsable MIME; the literal
            // `"application/octet-stream"` is hard-coded and a valid RFC-2045
            // token, so this arm is structurally unreachable.
            Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
        };
        form = form.part("file", file_part);
        client.post(&presigned_url).multipart(form).send()
    })?;

    Ok(identifier)
}

// ---------------------------------------------------------------------------
// publish_to_cloudsmith
// ---------------------------------------------------------------------------

/// Format the single default-verbosity summary line for one cloudsmith entry,
/// collapsing the per-file `uploading …` / `uploaded …` / `skipping …`
/// firehose into one line. `uploaded` counts artifacts this run newly landed;
/// `skipped` counts artifacts already present with a matching md5 (no upload
/// issued).
pub(crate) fn cloudsmith_upload_summary(
    uploaded: usize,
    skipped: usize,
    org: &str,
    repo: &str,
) -> String {
    format!("uploaded {uploaded} artifact(s), skipped {skipped} (already present) → {org}/{repo}")
}
