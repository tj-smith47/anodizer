use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
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
            let create_resp = client
                .post(&files_create_url)
                .header("Authorization", format!("token {}", token))
                .header("Accept", "application/json")
                .json(&files_create_body)
                .send()
                .with_context(|| {
                    format!(
                        "cloudsmith files/create transport error for '{}' at {}",
                        art_name, files_create_url
                    )
                })?;
            let create_status = create_resp.status();
            let create_body = create_resp
                .text()
                .unwrap_or_else(|e| format!("<failed to read body: {}>", e));
            if !create_status.is_success() {
                bail!(
                    "cloudsmith files/create for '{}' returned HTTP {}: {}",
                    art_name,
                    create_status,
                    create_body.trim()
                );
            }
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
            let mut form = reqwest::blocking::multipart::Form::new();
            for (k, v) in &upload_fields {
                let val = v
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string());
                form = form.text(k.clone(), val);
            }
            let file_part = reqwest::blocking::multipart::Part::bytes(file_bytes)
                .file_name(art_name.to_string())
                .mime_str("application/octet-stream")
                .context("cloudsmith: failed to build multipart file part")?;
            form = form.part("file", file_part);

            let upload_resp = client
                .post(&presigned_url)
                .multipart(form)
                .send()
                .with_context(|| {
                    format!(
                        "cloudsmith presigned upload transport error for '{}'",
                        art_name
                    )
                })?;
            let upload_status = upload_resp.status();
            if !upload_status.is_success() {
                let upload_body = upload_resp
                    .text()
                    .unwrap_or_else(|e| format!("<failed to read body: {}>", e));
                bail!(
                    "cloudsmith presigned upload for '{}' returned HTTP {}: {}",
                    art_name,
                    upload_status,
                    upload_body.trim()
                );
            }

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
            let pkg_resp = client
                .post(&package_upload_url)
                .header("Authorization", format!("token {}", token))
                .header("Accept", "application/json")
                .json(&package_body)
                .send()
                .with_context(|| {
                    format!(
                        "cloudsmith packages/upload/{} transport error for '{}' at {}",
                        fmt, art_name, package_upload_url
                    )
                })?;
            let pkg_status = pkg_resp.status();
            let pkg_body = pkg_resp
                .text()
                .unwrap_or_else(|e| format!("<failed to read body: {}>", e));
            if !pkg_status.is_success() {
                bail!(
                    "cloudsmith packages/upload/{} for '{}' returned HTTP {}: {}",
                    fmt,
                    art_name,
                    pkg_status,
                    pkg_body.trim()
                );
            }

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
}
