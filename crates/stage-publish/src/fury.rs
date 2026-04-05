use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{bail, Result};

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for Fury uploads: apk, deb, rpm.
pub fn fury_default_formats() -> Vec<&'static str> {
    vec!["apk", "deb", "rpm"]
}

/// Build the Fury push URL for the given account.
pub fn fury_push_url(account: &str) -> String {
    format!("https://push.fury.io/{}/", account)
}

/// Check if a filename matches any of the given format extensions.
pub fn fury_format_matches(filename: &str, formats: &[String]) -> bool {
    formats
        .iter()
        .any(|fmt| filename.ends_with(&format!(".{}", fmt)))
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
        let account = match entry.account.as_deref() {
            Some(a) if !a.is_empty() => a,
            _ => bail!("fury: 'account' is required but not set"),
        };

        // Resolve the secret env-var name (default: FURY_TOKEN).
        let secret_name = entry
            .secret_name
            .as_deref()
            .unwrap_or("FURY_TOKEN");

        // Render secret_name through template engine in case it contains
        // template expressions.
        let secret_name_rendered = ctx
            .render_template(secret_name)
            .unwrap_or_else(|_| secret_name.to_string());

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => fury_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        let push_url = fury_push_url(account);

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
            continue;
        }

        // --- Live mode ---
        // Resolve push token from environment.
        let token = std::env::var(&secret_name_rendered).map_err(|_| {
            anyhow::anyhow!(
                "fury: environment variable '{}' not set (needed for account '{}')",
                secret_name_rendered,
                account
            )
        })?;

        // Artifact iteration placeholder — no artifact registry in Context yet,
        // so we log and continue.  When the registry is wired, iterate over
        // matching artifacts and POST each to the push URL with Bearer auth.
        let _ = token; // suppress unused warning until artifact iteration is wired
        log.status(&format!(
            "fury: no artifacts to upload for account '{}' (artifact registry not yet implemented)",
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
    use anodize_core::config::{Config, FuryConfig, StringOrBool};
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
        assert_eq!(url, "https://push.fury.io/myaccount/");
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
            secret_name: None, // should default to FURY_TOKEN
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
        // First entry proceeds, second is skipped — both are ok
        assert!(publish_to_fury(&ctx, &log).is_ok());
    }
}
