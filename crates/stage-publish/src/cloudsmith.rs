use anodize_core::artifact::ArtifactKind;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{bail, Context as _, Result};
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

/// Build the CloudSmith upload URL for the given org, repo, format, and distribution.
pub fn cloudsmith_upload_url(org: &str, repo: &str, format: &str, distribution: &str) -> String {
    format!(
        "https://upload.cloudsmith.io/{}/{}/{}/{}",
        org, repo, format, distribution
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
            if s.is_disabled(|tmpl| ctx.render_template(tmpl)) {
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
        let secret_name_rendered = crate::util::resolve_secret_name(
            ctx, entry.secret_name.as_deref(), "CLOUDSMITH_TOKEN",
        );

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
        let component = entry.component.as_ref().map(|c| {
            ctx.render_template(c).unwrap_or_else(|_| c.clone())
        });

        // Check republish flag.
        let republish = entry.republish.as_ref().map(|r| {
            r.evaluates_to_true(|tmpl| ctx.render_template(tmpl))
        }).unwrap_or(false);

        // Collect matching artifacts.
        let artifacts: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| {
                let valid_kind = matches!(
                    a.kind,
                    ArtifactKind::LinuxPackage | ArtifactKind::Archive
                );
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
            let sample_url = cloudsmith_upload_url(&organization, &repository, "{format}", "{distribution}");
            log.status(&format!(
                "(dry-run) would upload packages to CloudSmith org '{}' repo '{}' at {}",
                organization, repository, sample_url
            ));
            log.status(&format!(
                "(dry-run) formats filter: {:?}",
                formats
            ));
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

        let client = reqwest::blocking::Client::builder()
            .user_agent("anodize/1.0")
            .build()
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

            // Look up distribution for this format
            let distro = distributions
                .get(fmt)
                .or_else(|| distributions.get(art_name))
                .cloned()
                .unwrap_or_default();

            // Build multipart form
            let file_bytes = std::fs::read(path)
                .with_context(|| format!("cloudsmith: failed to read '{}'", path.display()))?;

            let file_part = reqwest::blocking::multipart::Part::bytes(file_bytes)
                .file_name(art_name.to_string())
                .mime_str("application/octet-stream")
                .context("cloudsmith: failed to build multipart")?;

            let mut form = reqwest::blocking::multipart::Form::new()
                .part("package_file", file_part);

            if !distro.is_empty() {
                form = form.text("distribution", distro.clone());
            }
            if let Some(ref comp) = component {
                form = form.text("component", comp.clone());
            }
            if republish {
                form = form.text("republish", "true".to_string());
            }

            let upload_url = format!(
                "https://upload.cloudsmith.io/{}/{}/",
                organization, repository
            );

            log.status(&format!(
                "uploading {} ({}) to {}",
                art_name, fmt, upload_url
            ));

            let resp = client
                .post(&upload_url)
                .header("X-Api-Key", &token)
                .multipart(form)
                .send()
                .with_context(|| {
                    format!("cloudsmith: HTTP request failed for '{}'", art_name)
                })?;

            let status = resp.status();
            if !status.is_success() {
                let resp_body = resp.text().unwrap_or_default();
                bail!(
                    "cloudsmith: upload of '{}' to org '{}' repo '{}' failed: {} — {}",
                    art_name,
                    organization,
                    repository,
                    status,
                    resp_body
                );
            }

            log.status(&format!("uploaded {} ({})", art_name, status));
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
    use anodize_core::artifact::Artifact;
    use anodize_core::config::{CloudSmithConfig, Config, StringOrBool};
    use anodize_core::context::{Context, ContextOptions};
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
        let url = cloudsmith_upload_url("myorg", "myrepo", "deb", "ubuntu/focal");
        assert_eq!(
            url,
            "https://upload.cloudsmith.io/myorg/myrepo/deb/ubuntu/focal"
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
        assert!(cloudsmith_format_matches("myapp-1.0.0.x86_64.rpm", &formats));
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
