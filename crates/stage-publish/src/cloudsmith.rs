use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for CloudSmith uploads: apk, deb, rpm.
pub fn cloudsmith_default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Check if a filename matches any of the given format extensions.
///
/// The user-facing CloudSmith config (per Pro docs) uses `apk`, `deb`,
/// `rpm`, `src.rpm` as filter slugs. CloudSmith's API path slug for
/// `.apk` files is `alpine`, so users may write either spelling — both
/// are recognized here. `srpm` / `src.rpm` strip the dotted prefix when
/// matched against a `.src.rpm` filename (the dotted slug otherwise
/// won't match through the generic suffix helper).
pub fn cloudsmith_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    let lower = filename.to_ascii_lowercase();
    for fmt in formats {
        let raw = fmt.as_ref();
        let suffix = match raw {
            "alpine" => ".apk",
            "srpm" | "src.rpm" => ".src.rpm",
            other => {
                if lower.ends_with(&format!(".{}", other)) {
                    return true;
                }
                continue;
            }
        };
        if lower.ends_with(suffix) {
            return true;
        }
    }
    false
}

/// Cloudsmith API base URL (used for files/create and packages/upload/*).
const CLOUDSMITH_API_BASE: &str = "https://api.cloudsmith.io/v1";

/// Resolve the Cloudsmith API base URL. Defaults to [`CLOUDSMITH_API_BASE`];
/// `ANODIZE_CLOUDSMITH_API_BASE` overrides it so tests can point the 3-step
/// upload flow at a local responder without a real network call. The env
/// read is the only test seam — production runs never set the variable.
fn cloudsmith_api_base() -> String {
    std::env::var("ANODIZE_CLOUDSMITH_API_BASE").unwrap_or_else(|_| CLOUDSMITH_API_BASE.to_string())
}

/// Build the CloudSmith upload URL for the given org, repo, format, and distribution.
///
/// Retained for dry-run logging parity with prior versions. The live code
/// path uses the canonical 3-step API flow (files/create → S3 presigned
/// upload → packages/upload/{format}/) rather than this URL directly.
pub fn cloudsmith_upload_url(org: &str, repo: &str, format: &str, distribution: &str) -> String {
    format!(
        "{}/packages/{}/{}/upload/{}/ (distribution={})",
        CLOUDSMITH_API_BASE, org, repo, format, distribution
    )
}

/// Detect the package format from a filename extension.
///
/// Returns the CloudSmith API-side format slug (`alpine`, `deb`, `rpm`,
/// `srpm`, or `raw`). `.src.rpm` is matched BEFORE `.rpm` because the
/// suffix overlaps — CloudSmith treats source RPMs as a distinct format
/// at `/packages/<org>/<repo>/upload/srpm/`.
fn detect_format(filename: &str) -> &str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".src.rpm") {
        "srpm"
    } else if lower.ends_with(".deb") {
        "deb"
    } else if lower.ends_with(".rpm") {
        "rpm"
    } else if lower.ends_with(".apk") {
        "alpine"
    } else {
        "raw"
    }
}

/// CloudSmith API format slugs that accept a Debian `component:` field.
/// Other formats silently ignore `component`; the upload code drops it
/// to avoid noise in the request body.
const COMPONENT_BEARING_FORMATS: &[&str] = &["deb"];

/// Outcome of checking whether a package already exists on Cloudsmith.
/// Returned by [`check_cloudsmith_package_exists`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloudsmithPackageState {
    /// No package found with the given filename: caller should upload.
    NotFound,
    /// Package found with matching md5: caller should skip (idempotent).
    SkipIdempotent,
    /// Package found with a different md5: caller should bail loudly.
    /// `remote` is the md5 reported by Cloudsmith.
    Md5Mismatch { remote: String },
}

/// Classify a Cloudsmith packages-list response body against the local md5.
///
/// Pure function so the decision rule can be unit-tested without I/O.
/// Cloudsmith returns a JSON array of package objects; each entry has at
/// least `filename` and `checksum_md5`. We look for the first entry whose
/// `filename` matches `art_name` exactly.
///
/// Field names verified against the live Cloudsmith OpenAPI spec at
/// `https://api.cloudsmith.io/openapi/` — `Package` definition:
///
/// - `filename`: string (title "Filename")
/// - `checksum_md5`: string, readOnly
///
/// The packages_list endpoint (`GET /packages/{owner}/{repo}/`) returns
/// `type: array, items: $ref '#/definitions/Package'` — no envelope.
pub(crate) fn classify_cloudsmith_package_response(
    body: &str,
    art_name: &str,
    local_md5: &str,
) -> Result<CloudsmithPackageState> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("cloudsmith: parse packages-list body: {}", body.trim()))?;
    let array = match parsed.as_array() {
        Some(a) => a,
        None => return Ok(CloudsmithPackageState::NotFound),
    };
    for entry in array {
        let filename = entry.get("filename").and_then(|v| v.as_str()).unwrap_or("");
        if filename != art_name {
            continue;
        }
        let remote_md5 = entry
            .get("checksum_md5")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if remote_md5.is_empty() {
            // Package exists but Cloudsmith didn't report a checksum we can
            // verify. Treat as idempotent skip rather than upload-and-create
            // a duplicate: presence-by-filename is the strongest signal we have.
            return Ok(CloudsmithPackageState::SkipIdempotent);
        }
        if remote_md5 == local_md5.to_ascii_lowercase() {
            return Ok(CloudsmithPackageState::SkipIdempotent);
        }
        return Ok(CloudsmithPackageState::Md5Mismatch { remote: remote_md5 });
    }
    Ok(CloudsmithPackageState::NotFound)
}

/// GET the Cloudsmith packages-list endpoint filtered by filename and
/// classify the result. Retries 5xx/429/transport via the shared retry
/// helper; 4xx fast-fails.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_cloudsmith_package_exists(
    client: &reqwest::blocking::Client,
    list_url: &str,
    query: &str,
    token: &str,
    art_name: &str,
    local_md5: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<CloudsmithPackageState> {
    log.verbose(&format!(
        "cloudsmith: checking existing package for '{}' (query={})",
        art_name, query
    ));
    let result = retry_request("packages/list", art_name, policy, log, || {
        client
            .get(list_url)
            .query(&[("query", query), ("page_size", "100")])
            .header("Authorization", format!("token {}", token))
            .header("Accept", "application/json")
            .send()
    });
    let (_status, body) = match result {
        Ok(pair) => pair,
        Err(err) => {
            // Treat any failure to query as "unknown" — fall through to
            // upload rather than spuriously bail. The error has already been
            // shaped (and any bearer tokens redacted) by retry_request.
            log.warn(&format!(
                "cloudsmith: could not query existing packages for '{}' ({}); attempting upload anyway",
                art_name, err
            ));
            return Ok(CloudsmithPackageState::NotFound);
        }
    };
    classify_cloudsmith_package_response(&body, art_name, local_md5)
}

/// Retry an HTTP request builder, threading classification through the
/// shared [`retry_http_blocking`] helper. `build_send` is called per attempt
/// so multipart bodies can be rebuilt. 5xx/429 + transport errors retry;
/// 4xx fast-fails. Returns `(status, body)` on success.
fn retry_request<F>(
    label: &str,
    art_name: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    mut build_send: F,
) -> Result<(reqwest::StatusCode, String)>
where
    F: FnMut() -> Result<reqwest::blocking::Response, reqwest::Error>,
{
    let scope = format!("cloudsmith {label} for '{art_name}'");
    retry_http_blocking(
        &scope,
        policy,
        SuccessClass::Strict,
        |attempt| {
            if attempt > 1 {
                log.verbose(&format!(
                    "cloudsmith: retrying {label} for '{art_name}' (attempt {attempt})"
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
fn stage_cloudsmith_file(
    client: &reqwest::blocking::Client,
    api_base: &str,
    organization: &str,
    repository: &str,
    art_name: &str,
    md5_hex: &str,
    file_bytes: &[u8],
    token: &str,
    policy: &RetryPolicy,
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

    log.verbose(&format!("[step 1/3] POST {}", files_create_url));
    let (_create_status, create_body) =
        retry_request("files/create", art_name, policy, log, || {
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
    log.verbose(&format!("[step 2/3] POST {} (presigned)", presigned_url));
    // Multipart Form is move-only, so we rebuild it on every retry attempt.
    // Cloning `file_bytes` and `upload_fields` per-attempt is the price of
    // retriability; the bytes are already in memory.
    let _ = retry_request("presigned upload", art_name, policy, log, || {
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

/// Upload packages to CloudSmith via the CloudSmith API.
///
/// This is a top-level publisher: it reads from `ctx.config.cloudsmiths` rather
/// than from per-crate publish configs.  Each entry specifies an organization,
/// repository, optional credential env var, and optional format/distribution
/// filters.
///
/// Returns the list of [`CloudsmithTarget`]s actually uploaded this run, with
/// the `slug` (Cloudsmith's per-package permanent identifier) populated when
/// the step-3 `packages/upload/<format>/` response surfaced one. The returned
/// list drives `PublishEvidence::extra.cloudsmith_targets` so [`rollback`]
/// can issue real `DELETE /v1/packages/<org>/<repo>/<slug>/` calls; targets
/// whose slug couldn't be parsed degrade to the warn-only manual-cleanup
/// path (see [`cloudsmith_manual_cleanup_msg`]).
///
/// SkipIdempotent matches (artifact already present with matching md5) are
/// NOT included in the return — rollback's semantic is "undo what this run
/// uploaded," and a remote-side hit was put there by an earlier run.
pub(crate) fn publish_to_cloudsmith(
    ctx: &Context,
    log: &StageLogger,
) -> Result<Vec<CloudsmithTarget>> {
    let mut uploaded: Vec<CloudsmithTarget> = Vec::new();
    let entries = match ctx.config.cloudsmiths {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(uploaded),
    };

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every step of the 3-stage upload (files/create → S3 presigned →
    // packages/upload). Mirrors GoReleaser, where the retry policy is set
    // once per pipe invocation.
    let policy = ctx.retry_policy();

    for entry in entries {
        // Check skip flag.
        if let Some(ref s) = entry.skip {
            let off = s
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render skip template")?;
            if off {
                log.status("cloudsmith: entry skipped");
                continue;
            }
        }

        let proceed = anodizer_core::config::evaluate_if_condition(
            entry.if_condition.as_deref(),
            "cloudsmith entry",
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("cloudsmith: entry skipped — `if` condition evaluated falsy");
            continue;
        }

        // Organization is required — bail before dry-run so config errors
        // surface even in dry-run mode.
        let org_raw = match entry.organization.as_deref() {
            Some(o) if !o.is_empty() => o,
            _ => bail!("cloudsmith: 'organization' is required but not set"),
        };

        // Repository is required.
        let repo_raw = match entry.repository.as_deref() {
            Some(r) if !r.is_empty() => r,
            _ => bail!("cloudsmith: 'repository' is required but not set"),
        };

        // Render organization and repository through template engine in case
        // they contain template expressions.
        let organization = ctx
            .render_template(org_raw)
            .with_context(|| format!("cloudsmith: failed to render organization '{}'", org_raw))?;

        let repository = ctx
            .render_template(repo_raw)
            .with_context(|| format!("cloudsmith: failed to render repository '{}'", repo_raw))?;

        // Resolve the secret env-var name (default: CLOUDSMITH_TOKEN).
        let secret_name_rendered =
            crate::util::resolve_secret_name(ctx, entry.secret_name.as_deref(), "CLOUDSMITH_TOKEN");

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => cloudsmith_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        // Resolve distributions map (format -> Vec<distro string>). Each
        // entry yields one or more distribution slugs (the publisher
        // issues one upload per slug, GR Pro v2.8+ semantics). A
        // template-rendering failure on any slug is a config error and
        // hard-bails so a typo doesn't silently route an upload to the
        // wrong distribution.
        let distributions: HashMap<String, Vec<String>> = match entry.distributions {
            Some(ref d) => {
                let mut out: HashMap<String, Vec<String>> = HashMap::new();
                for (k, v) in d {
                    let raw_entries = v.as_slice();
                    let mut rendered_entries: Vec<String> = Vec::with_capacity(raw_entries.len());
                    for raw in raw_entries {
                        let rendered = ctx.render_template(raw).with_context(|| {
                            format!(
                                "cloudsmith: render distribution slug '{}' for format '{}'",
                                raw, k
                            )
                        })?;
                        rendered_entries.push(rendered);
                    }
                    out.insert(k.clone(), rendered_entries);
                }
                out
            }
            None => HashMap::new(),
        };

        // Resolve component (optional, used for deb).
        let component = entry
            .component
            .as_ref()
            .map(|c| ctx.render_template(c).unwrap_or_else(|_| c.clone()));

        // Check republish flag.
        let republish = match entry.republish.as_ref() {
            Some(r) => r
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render republish template")?,
            None => false,
        };

        // Collect matching artifacts.
        let artifacts: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| {
                let valid_kind =
                    matches!(a.kind, ArtifactKind::LinuxPackage | ArtifactKind::Archive);
                if !valid_kind {
                    return false;
                }
                if !cloudsmith_format_matches(a.name(), &formats) {
                    return false;
                }
                crate::util::matches_id_filter(a, entry.ids.as_deref())
            })
            .collect();

        // --- Dry-run logging ---
        if ctx.is_dry_run() {
            let sample_url =
                cloudsmith_upload_url(&organization, &repository, "{format}", "{distribution}");
            log.status(&format!(
                "(dry-run) would upload packages to CloudSmith org '{}' repo '{}' at {}",
                organization, repository, sample_url
            ));
            log.status(&format!("(dry-run) formats filter: {:?}", formats));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) build ID filter: {:?}", ids));
            }
            if !distributions.is_empty() {
                log.status(&format!("(dry-run) distributions: {:?}", distributions));
            }
            if let Some(ref comp) = component {
                log.status(&format!("(dry-run) component: {}", comp));
            }
            if republish {
                log.status("(dry-run) republish: true");
            }
            log.status(&format!(
                "(dry-run) credential env var: {}",
                secret_name_rendered
            ));
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            for a in &artifacts {
                log.status(&format!("(dry-run)   {} ({})", a.name(), a.kind));
            }
            continue;
        }

        // --- Live mode ---
        // Resolve token from environment.
        let token = ctx.env_var(&secret_name_rendered).ok_or_else(|| {
            anyhow!(
                "cloudsmith: environment variable '{}' not set (needed for org '{}' repo '{}')",
                secret_name_rendered,
                organization,
                repository
            )
        })?;

        if artifacts.is_empty() {
            log.status(&format!(
                "cloudsmith: no matching artifacts for org '{}' repo '{}' (formats: {:?})",
                organization, repository, formats
            ));
            continue;
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(60))
            .context("cloudsmith: failed to build HTTP client")?;

        log.status(&format!(
            "cloudsmith: uploading {} packages to org '{}' repo '{}'",
            artifacts.len(),
            organization,
            repository
        ));

        for artifact in &artifacts {
            let path = &artifact.path;
            if !path.exists() {
                bail!("cloudsmith: artifact file not found: {}", path.display());
            }

            let art_name = artifact.name();
            let fmt = detect_format(art_name);

            // Look up distribution(s) for this format. Cloudsmith accepts an
            // `any-distro/any-version` pseudo-entry for repos that aren't
            // distro-pinned, so an empty list is valid input and treated as
            // "no distribution override". The array form (GR Pro v2.8+)
            // produces one upload per slug.
            //
            // Routing is keyed on the API-side format slug (`apk`/`alpine`,
            // `deb`, `rpm`, `srpm`). The user-facing config key may be
            // either spelling — handle both so a config written against
            // GR docs (which use `apk`) and one written against
            // CloudSmith's API path (`alpine`) both work.
            let distro_slugs: Vec<String> = {
                let mut slugs: Vec<String> = distributions.get(fmt).cloned().unwrap_or_default();
                if slugs.is_empty() && fmt == "alpine" {
                    slugs = distributions.get("apk").cloned().unwrap_or_default();
                }
                if slugs.is_empty() && fmt == "srpm" {
                    slugs = distributions.get("src.rpm").cloned().unwrap_or_default();
                }
                slugs
            };

            let file_bytes = std::fs::read(path)
                .with_context(|| format!("cloudsmith: failed to read '{}'", path.display()))?;
            let size_bytes = file_bytes.len();

            // Cloudsmith's files/create API wants a hex-lowercase md5 of
            // the raw bytes.
            let md5_hex = {
                use md5::Digest as _;
                let mut hasher = md5::Md5::new();
                hasher.update(&file_bytes);
                anodizer_core::hashing::hex_lower(&hasher.finalize())
            };

            // Pre-check (republish=false only): query Cloudsmith for an
            // existing package with this filename. If found and md5
            // matches, skip (idempotent). If found but md5 differs,
            // bail — we can't fix the mismatch (the package is immutable
            // on Cloudsmith's side) and silently re-uploading produces
            // duplicate packages with different hashes.
            //
            // The `check_url` / `query` are built unconditionally so the
            // step-3 409-recovery path below can re-issue the same query
            // when an upload races against another concurrent CI loop
            // submitting the same package between pre-check and step-3.
            let api_base = cloudsmith_api_base();
            let check_url = format!("{}/packages/{}/{}/", api_base, organization, repository);
            let check_query = format!("filename:{}", art_name);
            if !republish {
                match check_cloudsmith_package_exists(
                    &client,
                    &check_url,
                    &check_query,
                    &token,
                    art_name,
                    &md5_hex,
                    &policy,
                    log,
                )? {
                    CloudsmithPackageState::SkipIdempotent => {
                        log.status(&format!(
                            "cloudsmith: skipping '{}' — already uploaded with matching md5",
                            art_name
                        ));
                        continue;
                    }
                    CloudsmithPackageState::Md5Mismatch { remote } => {
                        bail!(
                            "cloudsmith: '{}' already exists in org '{}' repo '{}' \
                             with a different md5 (remote={}, local={}). \
                             Re-uploading would create a conflicting duplicate. \
                             Set republish: true to force overwrite.",
                            art_name,
                            organization,
                            repository,
                            remote,
                            md5_hex
                        );
                    }
                    CloudsmithPackageState::NotFound => {}
                }
            }

            // Iterate at least once even when no distributions are
            // configured. An empty slug means "no distribution override"
            // (some repos are not distro-pinned).
            let upload_slugs: Vec<String> = if distro_slugs.is_empty() {
                vec![String::new()]
            } else {
                distro_slugs.clone()
            };

            log.status(&format!(
                "uploading {} ({}, {} bytes, md5={}) -> org '{}' repo '{}'{}",
                art_name,
                fmt,
                size_bytes,
                md5_hex,
                organization,
                repository,
                if distro_slugs.is_empty() {
                    String::new()
                } else {
                    format!(" distros={:?}", distro_slugs)
                },
            ));

            // --- Step 3/3 prep: package-create URL + component gating ---
            //
            // POST /v1/packages/{org}/{repo}/upload/{format}/ with the
            // identifier + distribution tells Cloudsmith to take the
            // uploaded raw file and register it as a deb/rpm/alpine
            // package. Without this step the bytes are dangling.
            //
            // When multiple distributions are configured (GR Pro v2.8+
            // array form), step 3 is issued once per slug — CloudSmith's
            // API accepts only one `distribution` per call. Each
            // files/create slot (`identifier`) is consumed by a single
            // package-create, so the file stage (steps 1+2) runs once PER
            // distribution inside the loop — reusing one identifier across
            // distributions 4xx's on the 2nd+ call (the slot is spent).
            let package_upload_url = format!(
                "{}/packages/{}/{}/upload/{}/",
                api_base, organization, repository, fmt
            );
            let component_for_format = component
                .as_ref()
                .filter(|_| COMPONENT_BEARING_FORMATS.contains(&fmt));
            if component.is_some() && component_for_format.is_none() {
                log.verbose(&format!(
                    "cloudsmith: component is set but format '{}' does not accept a component; dropping",
                    fmt
                ));
            }

            for distro in &upload_slugs {
                // Stage a fresh files/create slot + presigned upload for THIS
                // distribution. The identifier is single-use, so every
                // distribution needs its own.
                let identifier = stage_cloudsmith_file(
                    &client,
                    &api_base,
                    &organization,
                    &repository,
                    art_name,
                    &md5_hex,
                    &file_bytes,
                    &token,
                    &policy,
                    log,
                )?;

                let mut package_body = serde_json::json!({
                    "package_file": identifier,
                });
                if !distro.is_empty() {
                    package_body["distribution"] = serde_json::Value::String(distro.clone());
                }
                if let Some(comp) = component_for_format {
                    package_body["component"] = serde_json::Value::String(comp.clone());
                }
                if republish {
                    package_body["republish"] = serde_json::Value::Bool(true);
                }

                log.verbose(&format!(
                    "[step 3/3] POST {} (identifier={}, distro={:?})",
                    package_upload_url, identifier, distro
                ));
                let label = format!("packages/upload/{}", fmt);
                let step3_result = retry_request(&label, art_name, &policy, log, || {
                    client
                        .post(&package_upload_url)
                        .header("Authorization", format!("token {}", token))
                        .header("Accept", "application/json")
                        .json(&package_body)
                        .send()
                });

                let (pkg_status, pkg_body) = match step3_result {
                    Ok(pair) => pair,
                    Err(err) => {
                        // Race-recovery: a concurrent CI loop can submit the
                        // same name+version between our pre-check (or
                        // first-attempt step-3) and this step-3, returning
                        // 409/422 here. Without recovery, the upload aborts
                        // even though the operator's intent — "land this
                        // artifact on the registry" — was satisfied by the
                        // racing process. Re-query the remote: if it now
                        // exists with our md5, treat as idempotent skip; if
                        // it exists with a different md5, surface the same
                        // conflict the pre-check would have. Anything else
                        // (transport failure, 5xx after retries) propagates.
                        let status_in_chain: Option<u16> = err.chain().find_map(|e| {
                            e.downcast_ref::<anodizer_core::retry::HttpError>()
                                .map(|h| h.status)
                        });
                        let is_conflict = matches!(status_in_chain, Some(409) | Some(422));
                        if !is_conflict {
                            return Err(err);
                        }
                        log.warn(&format!(
                            "cloudsmith: step-3 returned {:?} for '{}'; re-checking remote to \
                             decide between idempotent skip and real conflict",
                            status_in_chain, art_name
                        ));
                        match check_cloudsmith_package_exists(
                            &client,
                            &check_url,
                            &check_query,
                            &token,
                            art_name,
                            &md5_hex,
                            &policy,
                            log,
                        )? {
                            CloudsmithPackageState::SkipIdempotent => {
                                let msg = format!(
                                    "cloudsmith: '{}' already landed with matching md5 \
                                     (concurrent uploader); treating as idempotent skip",
                                    art_name
                                );
                                if republish {
                                    log.warn(&msg);
                                } else {
                                    log.status(&msg);
                                }
                                continue;
                            }
                            CloudsmithPackageState::Md5Mismatch { remote } => {
                                bail!(
                                    "cloudsmith: step-3 conflict for '{}' in org '{}' repo \
                                     '{}'; remote md5={} differs from local={}. A concurrent \
                                     upload submitted different bytes under the same name. \
                                     Set republish: true to force overwrite, or bump the \
                                     release.",
                                    art_name,
                                    organization,
                                    repository,
                                    remote,
                                    md5_hex
                                );
                            }
                            CloudsmithPackageState::NotFound => {
                                return Err(err);
                            }
                        }
                    }
                };

                let slug = serde_json::from_str::<serde_json::Value>(&pkg_body)
                    .ok()
                    .and_then(|v| {
                        v.get("slug_perm")
                            .or_else(|| v.get("slug"))
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                    });
                if let Some(ref s) = slug {
                    log.status(&format!(
                        "uploaded {} (slug={}{})",
                        art_name,
                        s,
                        if distro.is_empty() {
                            String::new()
                        } else {
                            format!(", distro={}", distro)
                        }
                    ));
                } else {
                    log.status(&format!("uploaded {} (HTTP {})", art_name, pkg_status));
                }
                uploaded.push(CloudsmithTarget {
                    org: organization.clone(),
                    repo: repository.clone(),
                    filename: art_name.to_string(),
                    slug,
                });
            }
        }

        log.status(&format!(
            "cloudsmith: upload complete for org '{}' repo '{}'",
            organization, repository
        ));
    }

    Ok(uploaded)
}

// ---------------------------------------------------------------------------
// CloudsmithPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_cloudsmith`] in the [`anodizer_core::Publisher`] trait
// so the new dispatch path (see [`crate::registry::configured_publishers`])
// can drive Cloudsmith uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable packages,
// server-side deletable). `required = false`.
//
// Rollback shape: per uploaded package, issue
// `DELETE /v1/packages/<org>/<repo>/<slug>/` with the same `CLOUDSMITH_*`
// token used for the upload. The slug is the per-package permanent
// identifier returned by the step-3 `packages/upload/<format>/` response
// and is captured into `CloudsmithTarget.slug` so [`PublishEvidence::extra`]
// (`cloudsmith_targets` key) carries it across the publish/rollback split.
// Targets whose slug couldn't be parsed (older evidence written before
// B13, or a response-shape change) degrade to the warn-only manual-cleanup
// checklist via [`cloudsmith_manual_cleanup_msg`]. Per-target DELETE
// failures (non-2xx, transport errors) emit a warn and continue —
// rollback is best-effort and a single 5xx must not orphan the remaining
// packages.
simple_publisher!(
    CloudsmithPublisher,
    "cloudsmith",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("CLOUDSMITH_API_KEY package_delete"),
);

/// One Cloudsmith upload target as recorded in evidence. Operator-readable
/// `(org, repo, filename)` tuples drive the rollback warn line; the optional
/// `slug` (Cloudsmith's per-package permanent identifier, returned by the
/// step-3 `packages/upload/<format>/` response) lets [`rollback`] issue a
/// real `DELETE /v1/packages/<org>/<repo>/<slug>/` instead of a warn-only
/// manual-cleanup checklist.
///
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. `slug` stays `Option` because evidence
/// emitted before slug-capture didn't carry it; rollback falls back
/// to the warn-only path (see [`cloudsmith_manual_cleanup_msg`]) for
/// any target whose slug is absent.
pub(crate) type CloudsmithTarget = anodizer_core::publish_evidence::CloudsmithTargetSnapshot;

/// Encode the per-target tuples into the typed
/// [`PublishEvidenceExtra::Cloudsmith`] variant.
pub(crate) fn encode_cloudsmith_targets(
    targets: &[CloudsmithTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Cloudsmith(
        anodizer_core::publish_evidence::CloudsmithExtra {
            cloudsmith_targets: targets.to_vec(),
        },
    )
}

/// Decode the typed Cloudsmith variant back into structured targets.
/// Returns an empty vec when the variant doesn't match — the rollback
/// then surfaces the empty-evidence warn instead of crashing.
pub(crate) fn decode_cloudsmith_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<CloudsmithTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Cloudsmith(c) => c.cloudsmith_targets.clone(),
        _ => Vec::new(),
    }
}

/// The per-target warn line a rollback emits as a FALLBACK when no slug is
/// available in evidence (legacy evidence written before B13 added slug
/// capture, or a step-3 `packages/upload/<format>/` response that didn't
/// surface a slug). Operator-readable; renders the load-bearing
/// `<org>/<repo>` location plus the filename to remove. Exposed as a
/// helper so tests can pin the wording without intercepting stderr.
///
/// The PRIMARY rollback path issues a real
/// `DELETE /v1/packages/<org>/<repo>/<slug>/` against the Cloudsmith API
/// (see [`<CloudsmithPublisher as anodizer_core::Publisher>::rollback`]);
/// this helper is reached only when `target.slug` is `None`.
pub(crate) fn cloudsmith_manual_cleanup_msg(target: &CloudsmithTarget) -> String {
    format!(
        "cloudsmith: manually withdraw '{}' from {}/{} (per-package slug not surfaced in evidence; delete via the Cloudsmith dashboard)",
        target.filename, target.org, target.repo
    )
}

impl anodizer_core::Publisher for CloudsmithPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // The upload path returns the live target list (with slugs
        // populated when step 3's response carried one) so evidence
        // records what we actually uploaded — not a post-hoc walk of
        // config + artifacts, which can drift from the upload list and
        // never captures the slug. SkipIdempotent matches (artifact
        // already on Cloudsmith with matching md5) are NOT in `targets`
        // because rollback only undoes what THIS run did.
        let targets = publish_to_cloudsmith(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("cloudsmith");
        // The `artifact_paths` slot keeps the operator-readable
        // `<org>/<repo>/<filename>` form for the text-only
        // --rollback-only summary; the structured copy in `extra` is the
        // authoritative source for the DELETE call.
        let path_view: Vec<std::path::PathBuf> = targets
            .iter()
            .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
            .collect();
        if let Some(first) = path_view.first() {
            evidence.primary_ref = Some(first.display().to_string());
        }
        evidence.artifact_paths = path_view;
        evidence.extra = encode_cloudsmith_targets(&targets);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_cloudsmith_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "cloudsmith",
                "upload targets",
            ));
            return Ok(());
        }

        // Resolve the API token once; if it's absent we cannot DELETE
        // anything, so fall back to the warn-only manual-cleanup
        // checklist for every target. `CLOUDSMITH_API_KEY` is the
        // rollback-scope env name declared by `rollback_scope_needed`.
        let token = ctx.env_var("CLOUDSMITH_API_KEY");
        if token.is_none() {
            log.warn(
                "cloudsmith: CLOUDSMITH_API_KEY not set; emitting manual-cleanup checklist instead of DELETE",
            );
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
            .context("cloudsmith: failed to build HTTP client for rollback")?;
        let policy = ctx.retry_policy();

        let mut deleted = 0usize;
        let mut already_absent = 0usize;
        let mut failed = 0usize;
        let mut warn_only = 0usize;

        for target in &targets {
            // Two ways into the warn-only fallback:
            //   1. No token at all (handled above; warn already emitted).
            //   2. No slug for this target (older evidence, or step-3
            //      response shape change).
            let Some(slug) = target.slug.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };
            let Some(tok) = token.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };

            let url = format!(
                "{}/packages/{}/{}/{}/",
                CLOUDSMITH_API_BASE, target.org, target.repo, slug
            );
            log.status(&format!("cloudsmith: DELETE {}", url));
            let label = "packages/delete";
            match retry_request(label, &target.filename, &policy, &log, || {
                client
                    .delete(&url)
                    .header("Authorization", format!("token {}", tok))
                    .header("Accept", "application/json")
                    .send()
            }) {
                Ok((status, _body)) => {
                    if status.is_success() {
                        deleted += 1;
                    } else {
                        // `retry_http_blocking` Strict mode treats only
                        // 2xx as success, so 4xx (other than 404/410) and
                        // 5xx already raise an `Err` here. This arm is
                        // unreachable, but guard it defensively.
                        failed += 1;
                        log.warn(&format!(
                            "cloudsmith: DELETE {} returned HTTP {} (manual cleanup may be required)",
                            url, status
                        ));
                    }
                }
                Err(err) => {
                    // 404 / 410 = package already absent (operator deleted
                    // via the dashboard, or a prior partial rollback ran).
                    // Detect by substring on the shaped error message.
                    let msg = format!("{err:#}");
                    if msg.contains("HTTP 404") || msg.contains("HTTP 410") {
                        already_absent += 1;
                        log.status(&format!(
                            "cloudsmith: DELETE {} already absent (404/410)",
                            url
                        ));
                    } else {
                        failed += 1;
                        log.warn(&format!(
                            "cloudsmith: DELETE {} failed ({}); manual cleanup may be required",
                            url, err
                        ));
                    }
                }
            }
        }

        log.status(&format!(
            "cloudsmith: rollback complete — {} deleted, {} already absent, {} failed, {} warn-only (slug/token unavailable)",
            deleted, already_absent, failed, warn_only
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn skips_on_nightly(&self) -> bool {
        // Cloudsmith supports versioned packages; nightly uploads do not
        // clobber stable content and are allowed.
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::{CloudSmithConfig, Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use std::path::PathBuf;

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_cloudsmith_skips_when_no_config() {
        let config = Config::default();
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_empty_vec() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_skipped() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_skip_string_true() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            skip: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_requires_organization() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: None,
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'organization' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_organization_nonempty() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some(String::new()),
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'organization' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_repository() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'repository' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_repository_nonempty() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some(String::new()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'repository' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_upload_url() {
        // Display-only helper (dry-run logs). Live code uses the 3-step API
        // flow against api.cloudsmith.io, not a single upload URL.
        let url = cloudsmith_upload_url("myorg", "myrepo", "deb", "ubuntu/focal");
        assert_eq!(
            url,
            format!(
                "{}/packages/myorg/myrepo/upload/deb/ (distribution=ubuntu/focal)",
                CLOUDSMITH_API_BASE
            )
        );
    }

    #[test]
    fn test_cloudsmith_default_formats() {
        let defaults = cloudsmith_default_formats();
        assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
    }

    #[test]
    fn test_cloudsmith_dry_run() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_default_formats() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_ids_filter() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            ids: Some(vec!["build1".to_string(), "build2".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_distributions() {
        use anodizer_core::config::CloudSmithDistributions;

        let mut distributions = HashMap::new();
        distributions.insert(
            "deb".to_string(),
            CloudSmithDistributions::Single("ubuntu/focal".to_string()),
        );

        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            distributions: Some(distributions),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    /// YAML array form (`deb: ["ubuntu/focal", "ubuntu/jammy"]`) parses
    /// into [`CloudSmithDistributions::Multiple`] (GR Pro v2.8+).
    #[test]
    fn distributions_array_form_parses() {
        use anodizer_core::config::CloudSmithDistributions;
        let yaml = "deb:\n  - ubuntu/focal\n  - ubuntu/jammy\n";
        let parsed: HashMap<String, CloudSmithDistributions> =
            serde_yaml_ng::from_str(yaml).unwrap();
        match parsed.get("deb").unwrap() {
            CloudSmithDistributions::Multiple(v) => {
                assert_eq!(
                    v,
                    &vec!["ubuntu/focal".to_string(), "ubuntu/jammy".to_string()]
                );
            }
            other => panic!("expected Multiple, got {:?}", other),
        }
    }

    /// `.src.rpm` files map to the `srpm` format slug (NOT `rpm`).
    #[test]
    fn detect_format_distinguishes_src_rpm() {
        assert_eq!(detect_format("pkg-1.0-1.src.rpm"), "srpm");
        assert_eq!(detect_format("pkg-1.0-1.x86_64.rpm"), "rpm");
        assert_eq!(
            detect_format("pkg-1.0-1.SRC.rpm"),
            "srpm",
            "case-insensitive"
        );
    }

    /// `cloudsmith_format_matches` accepts both `apk` (user-facing) and
    /// `alpine` (API-side) spellings.
    #[test]
    fn format_matches_apk_and_alpine_aliases() {
        assert!(cloudsmith_format_matches("pkg.apk", &["apk".to_string()]));
        assert!(cloudsmith_format_matches(
            "pkg.apk",
            &["alpine".to_string()]
        ));
    }

    /// `cloudsmith_format_matches` recognises both `srpm` and `src.rpm`
    /// filter slugs against a `.src.rpm` file.
    #[test]
    fn format_matches_srpm_aliases() {
        assert!(cloudsmith_format_matches(
            "pkg-1.0-1.src.rpm",
            &["srpm".to_string()]
        ));
        assert!(cloudsmith_format_matches(
            "pkg-1.0-1.src.rpm",
            &["src.rpm".to_string()]
        ));
    }

    #[test]
    fn test_cloudsmith_dry_run_with_component() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            component: Some("main".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_republish() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_default_secret_name() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            secret_name: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_multiple_entries() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![
            CloudSmithConfig {
                organization: Some("org1".to_string()),
                repository: Some("repo1".to_string()),
                ..Default::default()
            },
            CloudSmithConfig {
                organization: Some("org2".to_string()),
                repository: Some("repo2".to_string()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        ]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_live_mode_errors_without_token() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            secret_name: Some("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345".to_string()),
            ..Default::default()
        }]);
        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let log = ctx.logger("cloudsmith");
        let result = publish_to_cloudsmith(&ctx, &log);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345"),
            "error should mention the secret env var name, got: {}",
            msg
        );
    }

    #[test]
    fn test_cloudsmith_format_matches() {
        let formats = vec!["deb".to_string(), "rpm".to_string()];
        assert!(cloudsmith_format_matches("myapp_1.0.0_amd64.deb", &formats));
        assert!(cloudsmith_format_matches(
            "myapp-1.0.0.x86_64.rpm",
            &formats
        ));
        assert!(!cloudsmith_format_matches("myapp-1.0.0.tar.gz", &formats));
    }

    #[test]
    fn test_cloudsmith_format_matches_apk() {
        let formats = vec!["apk".to_string()];
        assert!(cloudsmith_format_matches("myapp-1.0.0.apk", &formats));
        assert!(!cloudsmith_format_matches("myapp-1.0.0.deb", &formats));
    }

    #[test]
    fn test_cloudsmith_format_matches_empty_formats() {
        let formats: Vec<String> = vec![];
        assert!(!cloudsmith_format_matches("myapp.deb", &formats));
    }

    #[test]
    fn test_detect_format() {
        assert_eq!(detect_format("app.deb"), "deb");
        assert_eq!(detect_format("app.rpm"), "rpm");
        assert_eq!(detect_format("app.apk"), "alpine");
        assert_eq!(detect_format("app.tar.gz"), "raw");
    }

    #[test]
    fn test_cloudsmith_dry_run_lists_matching_artifacts() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "testapp_1.0.0_amd64.deb".to_string(),
            path: PathBuf::from("dist/testapp_1.0.0_amd64.deb"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "testapp-1.0.0.x86_64.rpm".to_string(),
            path: PathBuf::from("dist/testapp-1.0.0.x86_64.rpm"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    /// Defense-in-depth: a Cloudsmith API error response that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. Exercises the `retry_request`
    /// helper's error-message closure via a one-shot TCP responder.
    #[test]
    fn retry_request_redacts_bearer_in_error_body() {
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        use std::time::Duration;

        let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
        let body_len = leaky.len();
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );

        // Serve up to 3 identical attempts (matches fast_policy max_attempts).
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp; 3]);

        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let log = StageLogger::new("cloudsmith", Verbosity::Normal);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let url = format!("http://{addr}/files/");
        let err = retry_request("upload", "test.deb", &policy, &log, || {
            client.post(&url).send()
        })
        .expect_err("500 must exhaust + error");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "bearer token leaked into error chain: {chain}"
        );
        assert!(
            chain.contains("<redacted>"),
            "expected `<redacted>` marker in error chain: {chain}"
        );
    }

    /// Multi-distribution upload must stage a fresh files/create slot +
    /// presigned upload PER distribution: a Cloudsmith identifier is consumed
    /// by a single package-create, so reusing one across distributions makes
    /// the 2nd+ package-create 4xx.
    ///
    /// Two distributions ⇒ each needs its own (files/create + presigned +
    /// package-create) = 6 served connections. The bug (file stage hoisted
    /// out of the loop) would serve only 4 (1 files/create + 1 presigned +
    /// 2 package-creates). The connection count is the load-bearing assertion.
    #[test]
    #[serial_test::serial]
    fn cloudsmith_multi_distribution_stages_one_file_per_distro() {
        use anodizer_core::MapEnvSource;
        use anodizer_core::config::CloudSmithDistributions;
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let art_path = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art_path, b"fake-deb-bytes").unwrap();

        let http_json = |body: String| -> String {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
        };
        let presigned_ok = || "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string();

        // Build the response queue AFTER the responder binds so the
        // files/create `upload_url` can point the presigned upload (step 2)
        // back at this same responder. Served one-per-connection, in order;
        // per distribution the client opens three connections in sequence:
        //   files/create -> presigned upload -> packages/upload.
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            let base = format!("http://{addr}");
            let files_create = |id: &str| {
                http_json(format!(
                    r#"{{"identifier":"{id}","upload_url":"{base}/s3-presigned/","upload_fields":{{"key":"v"}}}}"#
                ))
            };
            vec![
                files_create("id-distro-1"),
                presigned_ok(),
                http_json(r#"{"slug_perm":"slug-1"}"#.to_string()),
                files_create("id-distro-2"),
                presigned_ok(),
                http_json(r#"{"slug_perm":"slug-2"}"#.to_string()),
            ]
        });
        let base = format!("http://{addr}");

        let mut distros: HashMap<String, CloudSmithDistributions> = HashMap::new();
        distros.insert(
            "deb".to_string(),
            CloudSmithDistributions::Multiple(vec![
                "ubuntu/focal".to_string(),
                "ubuntu/jammy".to_string(),
            ]),
        );

        let mut config = Config::default();
        config.project_name = "app".to_string();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            distributions: Some(distros),
            // republish=true skips the pre-check packages-list query so the
            // response queue stays exactly the 3-per-distro upload sequence.
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(
            MapEnvSource::new()
                .with("CLOUDSMITH_TOKEN", "fake-token")
                .with("ANODIZE_CLOUDSMITH_API_BASE", &base),
        );
        // `cloudsmith_api_base()` reads the process env (not ctx.env_var),
        // so the base override must be set there too. Serialized via #[serial].
        unsafe {
            std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base);
        }

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "app_1.0.0_amd64.deb".to_string(),
            path: art_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = StageLogger::new("cloudsmith", Verbosity::Quiet);
        let result = publish_to_cloudsmith(&ctx, &log);

        unsafe {
            std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE");
        }

        let uploaded = result.expect("multi-distribution upload should succeed");
        // One CloudsmithTarget recorded per distribution package-create.
        assert_eq!(
            uploaded.len(),
            2,
            "expected one recorded target per distribution, got {uploaded:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "two distributions must each stage their own file (3 connections \
             each: files/create + presigned + package-create); a hoisted file \
             stage would serve only 4"
        );
    }

    // ---- classify_cloudsmith_package_response ----------------------------
    //
    // Pure-function tests for the packages-list response classifier. The
    // network-bound `check_cloudsmith_package_exists` is exercised
    // indirectly via the same retry helper as `retry_request` (already
    // covered above); these tests pin the JSON decision rule.

    #[test]
    fn cloudsmith_classify_not_found_when_empty_array() {
        let result =
            classify_cloudsmith_package_response("[]", "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_not_found_when_no_matching_filename() {
        let body = r#"[{"filename":"other.deb","checksum_md5":"abcd"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_matches() {
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_matches_case_insensitive() {
        // Cloudsmith may return uppercase hex; our local computation is
        // lowercase. The comparator must normalize.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"DEADBEEF"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_field_absent() {
        // Filename match but no checksum_md5 in the response — presence is
        // a strong-enough idempotency signal; uploading would create a
        // duplicate package with a different md5.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_bails_when_md5_differs() {
        // The scenario the pre-check guards: a previous run uploaded with
        // one md5, the retry's re-packaged artifact has a different md5.
        // Bail loudly instead of creating a conflicting duplicate.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"aaaa1111"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(
            result,
            CloudsmithPackageState::Md5Mismatch {
                remote: "aaaa1111".to_string()
            }
        );
    }

    #[test]
    fn cloudsmith_classify_handles_non_array_body() {
        // An error envelope or unexpected shape: treat as NotFound rather
        // than blow up, since we can't fix the mismatch anyway and a false
        // upload-attempt is recoverable while a false bail is not.
        let body = r#"{"detail":"not authorized"}"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_picks_first_matching_filename() {
        // Defensive: if Cloudsmith returns multiple entries (e.g. across
        // distributions), the classifier picks the first match. Both
        // entries have the same md5 here, mirroring real-world behavior
        // where the same filename is shared across distros.
        let body = r#"[
            {"filename":"other.deb","checksum_md5":"abcd"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}
        ]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn cloudsmith_publisher_classification() {
        let p = CloudsmithPublisher::new();
        assert_eq!(p.name(), "cloudsmith");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("CLOUDSMITH_API_KEY package_delete")
        );
    }

    #[test]
    fn cloudsmith_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = CloudsmithPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn cloudsmith_rollback_warns_when_no_targets_recorded() {
        // Empty evidence drives rollback into the no-targets branch.
        // The capture pins that production actually invoked `log.warn`
        // with the helper-formatted message — a hand-constructed expected
        // string compared against the helper output would pass even if
        // the rollback body forgot the warn entirely.
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("cloudsmith");
        let p = CloudsmithPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("cloudsmith")
                && m.contains("upload targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Important #4 — per-target warn message renders a real cleanup
    /// instruction (org/repo/filename), not a fake URL.
    #[test]
    fn cloudsmith_manual_cleanup_msg_is_actionable() {
        let target = CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget_1.0.0_amd64.deb".to_string(),
            slug: None,
        };
        let msg = cloudsmith_manual_cleanup_msg(&target);
        assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
        assert!(msg.contains("acme/widget"), "{msg}");
        // The prior implementation rendered a `?filename=` URL — make
        // sure that shape can't sneak back in.
        assert!(!msg.contains("?filename="), "{msg}");
        assert!(!msg.contains("api.cloudsmith.io"), "{msg}");
    }

    /// Structured (org, repo, filename) tuples round-trip through
    /// PublishEvidence.extra so a future schema change cannot silently
    /// regress the rollback warn shape.
    #[test]
    fn cloudsmith_target_extra_roundtrips() {
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: None,
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: None,
            },
        ];
        let encoded = encode_cloudsmith_targets(&targets);
        let decoded = decode_cloudsmith_targets(&encoded);
        assert_eq!(decoded, targets);
    }

    // Slug captured at upload time round-trips through evidence so
    // rollback can issue real DELETEs. Also pins the wire-format key
    // for older anodize binaries decoding this evidence.
    #[test]
    fn cloudsmith_target_serde_roundtrip_with_slug() {
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: Some("aBcD1234".to_string()),
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: Some("xY9Z".to_string()),
            },
        ];
        let encoded = encode_cloudsmith_targets(&targets);
        let decoded = decode_cloudsmith_targets(&encoded);
        assert_eq!(decoded, targets);
        // Wire-format pin: serialize through evidence and inspect the
        // JSON to confirm the slug rides under the `cloudsmith_targets`
        // key (matches the pre-typed shape).
        let mut e = PublishEvidence::new("cloudsmith");
        e.extra = encoded;
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        let arr = v["extra"]["cloudsmith_targets"]
            .as_array()
            .expect("cloudsmith_targets array");
        let first = arr.first().expect("at least one entry");
        assert_eq!(first.get("slug").and_then(|s| s.as_str()), Some("aBcD1234"));
    }

    // Evidence written by versions before slug capture decodes with
    // `slug = None`, so rollback degrades cleanly to the warn-only
    // path. The snapshot's `#[serde(default)]` on `slug` powers this
    // wire-compat path.
    #[test]
    fn cloudsmith_target_decode_tolerates_missing_slug_field() {
        // Hand-rolled JSON matching the pre-slug-capture evidence shape
        // — wrapped in the `PublishEvidence` envelope so deserialization
        // exercises the same path live evidence files take.
        let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb"
                    },
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget-1.0.0-1.x86_64.rpm"
                    }
                ]
            }
        }"#;
        let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
        let decoded = decode_cloudsmith_targets(&e.extra);
        assert_eq!(decoded.len(), 2);
        assert!(
            decoded.iter().all(|t| t.slug.is_none()),
            "expected all slugs to decode as None for older evidence"
        );
        assert_eq!(decoded[0].filename, "widget_1.0.0_amd64.deb");
        assert_eq!(decoded[1].filename, "widget-1.0.0-1.x86_64.rpm");
    }

    // `null` slug values (the explicit serde shape when
    // `Option<String>` is None) also decode to `slug = None`.
    #[test]
    fn cloudsmith_target_decode_tolerates_null_slug() {
        let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb",
                        "slug": null
                    }
                ]
            }
        }"#;
        let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
        let decoded = decode_cloudsmith_targets(&e.extra);
        assert_eq!(decoded.len(), 1);
        assert!(decoded[0].slug.is_none());
    }

    #[test]
    fn cloudsmith_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence and assert (a) no
        // credential-shaped keys appear AND (b) the operator-public
        // upload coordinates serialize.
        let mut e = PublishEvidence::new("cloudsmith");
        e.extra = encode_cloudsmith_targets(&[CloudsmithTarget {
            org: "acme".into(),
            repo: "widget".into(),
            filename: "widget_1.0.0_amd64.deb".into(),
            slug: Some("aBcD1234".into()),
        }]);
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: org/repo/filename + slug present.
        assert!(s.contains("\"org\":\"acme\""), "{s}");
        assert!(s.contains("\"repo\":\"widget\""), "{s}");
        assert!(s.contains("\"filename\":\"widget_1.0.0_amd64.deb\""), "{s}");
        assert!(s.contains("\"slug\":\"aBcD1234\""), "{s}");
    }

    // B13 — rollback against evidence whose targets all lack a slug
    // (older `--rollback-only --from-run` replays, or step-3 responses
    // that omitted the slug field) returns Ok and never tries to issue
    // a DELETE against the Cloudsmith API. The `CLOUDSMITH_API_KEY` is
    // also absent here to make doubly sure no network call fires.
    #[test]
    fn cloudsmith_rollback_falls_back_to_warn_when_slug_missing() {
        // Inject an empty env source so `CLOUDSMITH_API_KEY` resolves
        // unset regardless of the ambient process env; the warn-only
        // path is forced for both the no-slug AND no-token reasons.
        let mut ctx = TestContextBuilder::new().build();
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: None,
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: None,
            },
        ];
        let mut evidence = PublishEvidence::new("cloudsmith");
        evidence.extra = encode_cloudsmith_targets(&targets);
        evidence.artifact_paths = targets
            .iter()
            .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
            .collect();

        let p = CloudsmithPublisher::new();
        assert!(
            p.rollback(&mut ctx, &evidence).is_ok(),
            "rollback must return Ok in warn-only fallback"
        );

        // Pin the exact warn-line shape so a refactor of
        // `cloudsmith_manual_cleanup_msg` can't silently regress the
        // operator instructions.
        let msg = cloudsmith_manual_cleanup_msg(&targets[0]);
        assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
        assert!(msg.contains("acme/widget"), "{msg}");
        assert!(msg.contains("per-package slug not surfaced"), "{msg}");
        assert!(msg.contains("Cloudsmith dashboard"), "{msg}");
    }
}
