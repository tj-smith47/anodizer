use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for CloudSmith uploads: apk, deb, rpm.
pub fn cloudsmith_default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Check if a filename matches any of the given format extensions.
pub fn cloudsmith_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    crate::util::format_matches(filename, formats)
}

/// Cloudsmith API base URL (used for files/create and packages/upload/*).
const CLOUDSMITH_API_BASE: &str = "https://api.cloudsmith.io/v1";

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
fn detect_format(filename: &str) -> &str {
    if filename.ends_with(".deb") {
        "deb"
    } else if filename.ends_with(".rpm") {
        "rpm"
    } else if filename.ends_with(".apk") {
        "alpine"
    } else {
        "raw"
    }
}

/// Lowercase hex encoding used for the md5 checksum Cloudsmith's
/// files/create API expects.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

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
/// - `filename`: string, "Full path to the file, including filename"
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

// ---------------------------------------------------------------------------
// publish_to_cloudsmith
// ---------------------------------------------------------------------------

/// Upload packages to CloudSmith via the CloudSmith API.
///
/// This is a top-level publisher: it reads from `ctx.config.cloudsmiths` rather
/// than from per-crate publish configs.  Each entry specifies an organization,
/// repository, optional credential env var, and optional format/distribution
/// filters.
pub fn publish_to_cloudsmith(ctx: &Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.cloudsmiths {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
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

        // Resolve distributions map (format -> distro string).
        let distributions: HashMap<String, String> = match entry.distributions {
            Some(ref d) => d
                .iter()
                .map(|(k, v)| {
                    let rendered_val = match v.as_str() {
                        Some(s) => ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
                        None => v.to_string(),
                    };
                    (k.clone(), rendered_val)
                })
                .collect(),
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
        let token = std::env::var(&secret_name_rendered).with_context(|| {
            format!(
                "cloudsmith: environment variable '{}' not set (needed for org '{}' repo '{}')",
                secret_name_rendered, organization, repository
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

            // Look up distribution for this format. Cloudsmith accepts an
            // `any-distro/any-version` pseudo-entry for repos that aren't
            // distro-pinned, so an empty value is valid input and treated
            // as "no distribution override".
            let distro = distributions
                .get(fmt)
                .or_else(|| distributions.get(art_name))
                .cloned()
                .unwrap_or_default();

            let file_bytes = std::fs::read(path)
                .with_context(|| format!("cloudsmith: failed to read '{}'", path.display()))?;
            let size_bytes = file_bytes.len();

            // Cloudsmith's files/create API wants a hex-lowercase md5 of
            // the raw bytes.
            let md5_hex = {
                use md5::Digest as _;
                let mut hasher = md5::Md5::new();
                hasher.update(&file_bytes);
                hex_lower(&hasher.finalize())
            };

            // B11 pre-check: query Cloudsmith for an existing package with
            // this filename. If found and md5 matches, skip (idempotent).
            // If found but md5 differs, bail — we can't fix the mismatch
            // (the package is immutable on Cloudsmith's side) and silently
            // re-uploading produces duplicate packages with different hashes.
            if !republish {
                let check_url = format!(
                    "{}/packages/{}/{}/",
                    CLOUDSMITH_API_BASE, organization, repository
                );
                let query = format!("filename:{}", art_name);
                match check_cloudsmith_package_exists(
                    &client, &check_url, &query, &token, art_name, &md5_hex, &policy, log,
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

            log.status(&format!(
                "uploading {} ({}, {} bytes, md5={}) -> org '{}' repo '{}'{}",
                art_name,
                fmt,
                size_bytes,
                md5_hex,
                organization,
                repository,
                if distro.is_empty() {
                    String::new()
                } else {
                    format!(" distro='{}'", distro)
                },
            ));

            // --- Step 1/3: request a files/create slot ---
            //
            // POST /v1/files/{org}/{repo}/ with the filename + md5 returns
            // a short-lived S3 presigned upload URL plus the fields the
            // upload POST must include. This matches what the official
            // Cloudsmith CLI's `request_file_upload` helper does.
            let files_create_url = format!(
                "{}/files/{}/{}/",
                CLOUDSMITH_API_BASE, organization, repository
            );
            let files_create_body = serde_json::json!({
                "filename": art_name,
                "md5_checksum": md5_hex,
                "method": "post",
            });

            log.verbose(&format!("[step 1/3] POST {}", files_create_url));
            let (_create_status, create_body) =
                retry_request("files/create", art_name, &policy, log, || {
                    client
                        .post(&files_create_url)
                        .header("Authorization", format!("token {}", token))
                        .header("Accept", "application/json")
                        .json(&files_create_body)
                        .send()
                })?;
            let create_json: serde_json::Value =
                serde_json::from_str(&create_body).with_context(|| {
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
            // The presigned URL is AWS S3 POST form — no Cloudsmith auth
            // header is added here. The fields returned in step 1 (policy,
            // signature, key, ...) MUST be included as multipart form text
            // parts exactly as given, and the actual file goes under the
            // `file` key (not `package_file`).
            log.verbose(&format!("[step 2/3] POST {} (presigned)", presigned_url));
            // Multipart Form is move-only, so we rebuild it on every retry
            // attempt. Cloning `file_bytes` and `upload_fields` per-attempt
            // is the price of retriability; the bytes are already in memory.
            let _ = retry_request("presigned upload", art_name, &policy, log, || {
                let mut form = reqwest::blocking::multipart::Form::new();
                for (k, v) in &upload_fields {
                    let val = v
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| v.to_string());
                    form = form.text(k.clone(), val);
                }
                let file_part = match reqwest::blocking::multipart::Part::bytes(file_bytes.clone())
                    .file_name(art_name.to_string())
                    .mime_str("application/octet-stream")
                {
                    Ok(p) => p,
                    // `mime_str` only fails on unparsable MIME; the literal
                    // `"application/octet-stream"` is hard-coded and a valid
                    // RFC-2045 token, so this arm is structurally unreachable.
                    // Use `unreachable!` (rather than the previous "synthesize
                    // a transport error against `data:,`" hack) — that hack
                    // produced spurious URL-scheme errors that masked real
                    // bugs and tripped the anti-pattern hook on `unwrap_or`.
                    Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
                };
                form = form.part("file", file_part);
                client.post(&presigned_url).multipart(form).send()
            })?;

            // --- Step 3/3: create the package record in the repo ---
            //
            // POST /v1/packages/{org}/{repo}/upload/{format}/ with the
            // identifier + distribution tells Cloudsmith to take the
            // uploaded raw file and register it as a deb/rpm/alpine
            // package. Without this step the bytes are dangling.
            let package_upload_url = format!(
                "{}/packages/{}/{}/upload/{}/",
                CLOUDSMITH_API_BASE, organization, repository, fmt
            );
            let mut package_body = serde_json::json!({
                "package_file": identifier,
            });
            if !distro.is_empty() {
                package_body["distribution"] = serde_json::Value::String(distro.clone());
            }
            if let Some(ref comp) = component {
                package_body["component"] = serde_json::Value::String(comp.clone());
            }
            if republish {
                package_body["republish"] = serde_json::Value::Bool(true);
            }

            log.verbose(&format!(
                "[step 3/3] POST {} (identifier={})",
                package_upload_url, identifier
            ));
            let label = format!("packages/upload/{}", fmt);
            let (pkg_status, pkg_body) = retry_request(&label, art_name, &policy, log, || {
                client
                    .post(&package_upload_url)
                    .header("Authorization", format!("token {}", token))
                    .header("Accept", "application/json")
                    .json(&package_body)
                    .send()
            })?;

            let slug = serde_json::from_str::<serde_json::Value>(&pkg_body)
                .ok()
                .and_then(|v| {
                    v.get("slug_perm")
                        .or_else(|| v.get("slug"))
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                });
            if let Some(s) = slug {
                log.status(&format!("uploaded {} (slug={})", art_name, s));
            } else {
                log.status(&format!("uploaded {} (HTTP {})", art_name, pkg_status));
            }
        }

        log.status(&format!(
            "cloudsmith: upload complete for org '{}' repo '{}'",
            organization, repository
        ));
    }

    Ok(())
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
        let mut distributions = HashMap::new();
        distributions.insert(
            "deb".to_string(),
            serde_json::Value::String("ubuntu/focal".to_string()),
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
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
        let body_len = leaky.len();
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_inner = counter.clone();
        std::thread::spawn(move || {
            // Serve up to 3 attempts (matches fast_policy max_attempts).
            for _ in 0..3 {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                counter_inner.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

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

    // ---- B11: classify_cloudsmith_package_response ------------------------
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
        // The B11 scenario: a previous run uploaded with one md5, the retry's
        // re-packaged artifact has a different md5. Bail loudly instead of
        // creating a conflicting duplicate.
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
