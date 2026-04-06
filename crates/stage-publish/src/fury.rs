use anodize_core::artifact::ArtifactKind;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{bail, Context as _, Result};

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for Fury uploads: apk, deb, rpm.
pub fn fury_default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Build the Fury push URL for the given account.
pub fn fury_push_url(account: &str) -> String {
    format!("https://push.fury.io/v1/{}/", account)
}

/// Check if a filename matches any of the given format extensions.
pub fn fury_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    crate::util::format_matches(filename, formats)
}

// ---------------------------------------------------------------------------
// publish_to_fury
// ---------------------------------------------------------------------------

/// Push deb/rpm/apk packages to GemFury.
///
/// This is a top-level publisher: it reads from `ctx.config.fury` rather than
/// from per-crate publish configs.  Each entry specifies an account, optional
/// credential env var, and optional format/ID filters.
pub fn publish_to_fury(ctx: &Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.fury {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    for entry in entries {
        // Check disable flag.
        if let Some(ref d) = entry.disable {
            if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                log.status("fury: entry disabled, skipping");
                continue;
            }
        }

        // Account is required — bail before dry-run so config errors surface
        // even in dry-run mode.
        let account_raw = match entry.account.as_deref() {
            Some(a) if !a.is_empty() => a,
            _ => bail!("fury: 'account' is required but not set"),
        };

        // Render account through template engine in case it contains
        // template expressions (e.g. `{{ .Env.FURY_ACCOUNT }}`).
        let account = ctx.render_template(account_raw)
            .with_context(|| format!("fury: failed to render account '{}'", account_raw))?;

        // Resolve the secret env-var name (default: FURY_TOKEN).
        let secret_name_rendered = crate::util::resolve_secret_name(
            ctx, entry.secret_name.as_deref(), "FURY_TOKEN",
        );

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => fury_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        let push_url = fury_push_url(&account);

        // Collect matching artifacts: LinuxPackage artifacts matching format filter.
        // Also check Archive artifacts for format matches (e.g. .deb in archives).
        let artifacts: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| {
                // Only package-like artifacts
                let valid_kind = matches!(
                    a.kind,
                    ArtifactKind::LinuxPackage | ArtifactKind::Archive
                );
                if !valid_kind {
                    return false;
                }
                // Must match format filter
                if !fury_format_matches(a.name(), &formats) {
                    return false;
                }
                // ID filter
                crate::util::matches_id_filter(a, entry.ids.as_deref())
            })
            .collect();

        // --- Dry-run logging ---
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would push packages to GemFury account '{}' at {}",
                account, push_url
            ));
            log.status(&format!(
                "(dry-run) formats filter: {:?}",
                formats
            ));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) build ID filter: {:?}", ids));
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
        // Resolve push token from environment.
        let token = std::env::var(&secret_name_rendered).with_context(|| {
            format!(
                "fury: environment variable '{}' not set (needed for account '{}')",
                secret_name_rendered,
                account
            )
        })?;

        if artifacts.is_empty() {
            log.status(&format!(
                "fury: no matching artifacts for account '{}' (formats: {:?})",
                account, formats
            ));
            continue;
        }

        let client = reqwest::blocking::Client::builder()
            .user_agent("anodize/1.0")
            .build()
            .context("fury: failed to build HTTP client")?;

        log.status(&format!(
            "fury: pushing {} packages to account '{}'",
            artifacts.len(),
            account
        ));

        for artifact in &artifacts {
            let path = &artifact.path;
            if !path.exists() {
                bail!("fury: artifact file not found: {}", path.display());
            }

            let body = std::fs::read(path)
                .with_context(|| format!("fury: failed to read '{}'", path.display()))?;

            log.status(&format!(
                "pushing {} ({} bytes) to {}",
                artifact.name(),
                body.len(),
                push_url
            ));

            let resp = client
                .put(&push_url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", body.len().to_string())
                .body(body)
                .send()
                .with_context(|| {
                    format!("fury: HTTP request failed for '{}'", artifact.name())
                })?;

            let status = resp.status();
            if !status.is_success() {
                let resp_body = resp.text().unwrap_or_default();
                bail!(
                    "fury: upload of '{}' to account '{}' failed: {} — {}",
                    artifact.name(),
                    account,
                    status,
                    resp_body
                );
            }

            log.status(&format!("pushed {} ({})", artifact.name(), status));
        }

        log.status(&format!(
            "fury: push complete for account '{}'",
            account
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
    use anodize_core::config::{Config, FuryConfig, StringOrBool};
    use anodize_core::context::{Context, ContextOptions};
    use std::collections::HashMap;
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
    fn test_fury_skips_when_no_config() {
        let config = Config::default();
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_skips_when_empty_vec() {
        let mut config = Config::default();
        config.fury = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_skips_when_disabled() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            disable: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_skips_when_disabled_string_true() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            disable: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_errors_when_account_missing() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            account: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        let err = publish_to_fury(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'account' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_fury_errors_when_account_empty() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            account: Some(String::new()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        let err = publish_to_fury(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'account' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_fury_default_formats() {
        let defaults = fury_default_formats();
        assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
    }

    #[test]
    fn test_fury_upload_url() {
        let url = fury_push_url("myaccount");
        assert_eq!(url, "https://push.fury.io/v1/myaccount/");
    }

    #[test]
    fn test_fury_filters_by_format() {
        let formats = vec!["deb".to_string(), "rpm".to_string()];
        assert!(fury_format_matches("myapp_1.0.0_amd64.deb", &formats));
        assert!(fury_format_matches("myapp-1.0.0.x86_64.rpm", &formats));
        assert!(!fury_format_matches("myapp-1.0.0.tar.gz", &formats));
    }

    #[test]
    fn test_fury_format_matches_apk() {
        let formats = vec!["apk".to_string()];
        assert!(fury_format_matches("myapp-1.0.0.apk", &formats));
        assert!(!fury_format_matches("myapp-1.0.0.deb", &formats));
    }

    #[test]
    fn test_fury_format_matches_empty_formats() {
        let formats: Vec<String> = vec![];
        assert!(!fury_format_matches("myapp.deb", &formats));
    }

    #[test]
    fn test_fury_dry_run() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            account: Some("myaccount".to_string()),
            secret_name: Some("MY_FURY_TOKEN".to_string()),
            formats: Some(vec!["deb".to_string(), "rpm".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_dry_run_with_ids_filter() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            account: Some("myaccount".to_string()),
            ids: Some(vec!["build1".to_string(), "build2".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_dry_run_default_secret_name() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig {
            account: Some("myaccount".to_string()),
            secret_name: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_multiple_entries() {
        let mut config = Config::default();
        config.fury = Some(vec![
            FuryConfig {
                account: Some("account1".to_string()),
                ..Default::default()
            },
            FuryConfig {
                account: Some("account2".to_string()),
                disable: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        ]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("fury");
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }

    #[test]
    fn test_fury_live_mode_errors_without_token() {
        let mut config = Config::default();
        let unique_secret = "FURY_TEST_TOKEN_SHOULD_NOT_EXIST_8f3a2c";
        config.fury = Some(vec![FuryConfig {
            account: Some("liveaccount".to_string()),
            secret_name: Some(unique_secret.to_string()),
            ..Default::default()
        }]);
        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let log = ctx.logger("fury");
        let err = publish_to_fury(&ctx, &log).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(unique_secret),
            "error should mention the secret env var name, got: {}",
            msg
        );
        assert!(
            msg.contains("liveaccount"),
            "error should mention the account, got: {}",
            msg
        );
    }

    #[test]
    fn test_fury_dry_run_lists_matching_artifacts() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        config.fury = Some(vec![FuryConfig {
            account: Some("myaccount".to_string()),
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
            kind: ArtifactKind::Archive,
            name: "testapp-1.0.0.tar.gz".to_string(),
            path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = ctx.logger("fury");
        // Should succeed, matching only the .deb file
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }
}
