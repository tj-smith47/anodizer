use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{bail, Context as _, Result};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for CloudSmith uploads: apk, deb, rpm.
pub fn cloudsmith_default_formats() -> Vec<&'static str> {
    vec!["apk", "deb", "rpm"]
}

/// Check if a filename matches any of the given format extensions.
pub fn cloudsmith_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    formats
        .iter()
        .any(|fmt| filename.ends_with(&format!(".{}", fmt.as_ref())))
}

/// Build the CloudSmith upload URL for the given org, repo, format, and distribution.
pub fn cloudsmith_upload_url(org: &str, repo: &str, format: &str, distribution: &str) -> String {
    format!(
        "https://upload.cloudsmith.io/{}/{}/{}/{}",
        org, repo, format, distribution
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
        let secret_name = entry
            .secret_name
            .as_deref()
            .unwrap_or("CLOUDSMITH_TOKEN");

        // Render secret_name through template engine in case it contains
        // template expressions.
        let secret_name_rendered = ctx
            .render_template(secret_name)
            .unwrap_or_else(|_| secret_name.to_string());

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => cloudsmith_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

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
            if let Some(ref distributions) = entry.distributions {
                let rendered: HashMap<&str, String> = distributions
                    .iter()
                    .map(|(k, v)| {
                        let rendered_val = match v.as_str() {
                            Some(s) => ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
                            None => v.to_string(),
                        };
                        (k.as_str(), rendered_val)
                    })
                    .collect();
                log.status(&format!("(dry-run) distributions: {:?}", rendered));
            }
            if let Some(ref component) = entry.component {
                let rendered_component = ctx
                    .render_template(component)
                    .unwrap_or_else(|_| component.to_string());
                log.status(&format!("(dry-run) component: {}", rendered_component));
            }
            if let Some(ref republish) = entry.republish {
                log.status(&format!(
                    "(dry-run) republish: {}",
                    republish.evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                ));
            }
            log.status(&format!(
                "(dry-run) credential env var: {}",
                secret_name_rendered
            ));
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

        // Artifact iteration placeholder — no artifact registry in Context yet,
        // so we log and continue.  When the registry is wired, iterate over
        // matching artifacts and POST each to the upload URL with Bearer auth.
        let _ = token; // suppress unused warning until artifact iteration is wired
        log.status(&format!(
            "cloudsmith: no artifacts to upload for org '{}' repo '{}' (artifact registry not yet implemented)",
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
    use anodize_core::config::{CloudSmithConfig, Config, StringOrBool};
    use anodize_core::context::{Context, ContextOptions};

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
        assert!(url.contains("myorg"));
        assert!(url.contains("myrepo"));
        assert!(url.contains("deb"));
        assert!(url.contains("ubuntu/focal"));
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
            formats: None, // should use defaults: apk, deb, rpm
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
            secret_name: None, // should default to CLOUDSMITH_TOKEN
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
        // First entry proceeds, second is skipped — both are ok
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
}
