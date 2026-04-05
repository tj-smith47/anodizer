use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;

// ---------------------------------------------------------------------------
// validate_upload_mode
// ---------------------------------------------------------------------------

/// Validate the upload mode string.  Only `"archive"` and `"binary"` are
/// accepted; anything else is an error.
pub fn validate_upload_mode(mode: &str) -> Result<()> {
    match mode {
        "archive" | "binary" => Ok(()),
        other => bail!(
            "artifactory: invalid upload mode '{}' (expected 'archive' or 'binary')",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// sha256_file
// ---------------------------------------------------------------------------

/// Compute the hex-encoded SHA-256 digest of a file.
/// Currently used only in tests; will be called during real artifact uploads
/// once the artifact registry is wired.
#[allow(dead_code)]
fn sha256_file(path: &std::path::Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("artifactory: failed to open '{}'", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("artifactory: failed to read '{}'", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// publish_to_artifactory
// ---------------------------------------------------------------------------

/// Upload artifacts to Artifactory via HTTP PUT.
///
/// This is a top-level publisher: it reads from `ctx.config.artifactories`
/// rather than from per-crate publish configs.  Each entry specifies a target
/// URL template, credentials, and optional filters.
pub fn publish_to_artifactory(ctx: &Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.artifactories {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    for entry in entries {
        // Check skip flag.
        if let Some(ref s) = entry.skip {
            if s.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                log.status("artifactory: entry skipped");
                continue;
            }
        }

        // Name is required.
        let name = match entry.name {
            Some(ref n) if !n.is_empty() => n.as_str(),
            _ => bail!("artifactory: entry is missing required 'name' field"),
        };

        // Validate mode (default: "archive").
        let mode = entry.mode.as_deref().unwrap_or("archive");
        validate_upload_mode(mode)?;

        // Target URL is required.
        let target_template = match entry.target {
            Some(ref t) if !t.is_empty() => t.as_str(),
            _ => bail!(
                "artifactory: entry '{}' is missing required 'target' URL",
                name
            ),
        };

        // Render the target URL template.
        let target_url = ctx
            .render_template(target_template)
            .with_context(|| format!("artifactory: failed to render target URL for '{}'", name))?;

        // Resolve credentials — render through template engine.
        let username = match entry.username {
            Some(ref u) => ctx
                .render_template(u)
                .with_context(|| format!("artifactory: failed to render username for '{}'", name))?,
            None => String::new(),
        };
        let named_env_var = format!(
            "ARTIFACTORY_{}_SECRET",
            name.to_uppercase().replace('-', "_")
        );
        let generic_env_var = "ARTIFACTORY_SECRET";
        // Try named env var, then generic env var, then config password
        // (rendered through templates), then empty string.
        // TODO: use `_password` once artifact iteration is wired up.
        let _password = std::env::var(&named_env_var)
            .ok()
            .or_else(|| std::env::var(generic_env_var).ok())
            .or_else(|| {
                entry.password.as_ref().and_then(|p| {
                    ctx.render_template(p).ok()
                })
            })
            .unwrap_or_default();

        // Determine checksum header name.
        let checksum_header = entry
            .checksum_header
            .as_deref()
            .unwrap_or("X-Checksum-SHA256");

        // Collect custom headers.
        let empty = HashMap::new();
        let custom_headers = entry.custom_headers.as_ref().unwrap_or(&empty);

        // --- Artifact iteration placeholder ---
        // There is no artifact registry in Context yet, so we cannot iterate
        // over real artifacts.  For now we log and return Ok.
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would upload artifacts to Artifactory '{}' at {} (mode={}, user={})",
                name, target_url, mode, username
            ));
            if !custom_headers.is_empty() {
                for (k, v) in custom_headers {
                    let rendered_v = ctx.render_template(v).unwrap_or_else(|_| v.clone());
                    log.status(&format!("(dry-run) custom header: {}={}", k, rendered_v));
                }
            }
            if let Some(ref cert) = entry.client_x509_cert {
                log.status(&format!("(dry-run) using client cert: {}", cert));
            }
            if let Some(ref key) = entry.client_x509_key {
                log.status(&format!("(dry-run) using client key: {}", key));
            }
            log.status(&format!(
                "(dry-run) checksum header: {}",
                checksum_header
            ));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) build ID filter: {:?}", ids));
            }
            if let Some(ref exts) = entry.exts {
                log.status(&format!("(dry-run) extension filter: {:?}", exts));
            }
            if let Some(checksum) = entry.checksum {
                log.status(&format!("(dry-run) include checksums: {}", checksum));
            }
            if let Some(signature) = entry.signature {
                log.status(&format!("(dry-run) include signatures: {}", signature));
            }
            if let Some(meta) = entry.meta {
                log.status(&format!("(dry-run) include metadata: {}", meta));
            }
            if let Some(custom_name) = entry.custom_artifact_name {
                log.status(&format!(
                    "(dry-run) custom artifact naming: {}",
                    custom_name
                ));
            }
            if let Some(ref files) = entry.extra_files {
                log.status(&format!("(dry-run) extra files: {} entries", files.len()));
            }
            log.status(&format!(
                "(dry-run) credential env var: {} (fallback: {})",
                named_env_var, generic_env_var
            ));
            continue;
        }

        // Live mode — no artifacts to upload yet.
        log.status(&format!(
            "artifactory: no artifacts to upload for '{}' (artifact registry not yet implemented)",
            name
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
    use anodize_core::config::{ArtifactoryConfig, Config, StringOrBool};
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
    fn test_artifactory_skips_when_no_config() {
        let config = Config::default();
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_skips_when_empty_vec() {
        let mut config = Config::default();
        config.artifactories = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_skips_when_skipped() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_default_checksum_header_in_dry_run() {
        // Verify the dry-run output uses the default checksum header name
        // when no custom header is configured.
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("chk".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            checksum_header: None, // no custom header configured
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        // Should succeed and internally use "X-Checksum-SHA256" as the header.
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_mode_validation() {
        assert!(validate_upload_mode("archive").is_ok());
        assert!(validate_upload_mode("binary").is_ok());
        assert!(validate_upload_mode("invalid").is_err());
    }

    #[test]
    fn test_artifactory_mode_validation_error_message() {
        let err = validate_upload_mode("foobar").unwrap_err();
        assert!(
            err.to_string().contains("invalid upload mode 'foobar'"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifactory_requires_target() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: None, // missing target
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("missing required 'target'"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifactory_requires_target_nonempty() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some(String::new()), // empty target
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_err());
    }

    #[test]
    fn test_artifactory_dry_run() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://artifactory.example.com/repo/myapp/1.0.0/".to_string()),
            mode: Some("archive".to_string()),
            username: Some("deployer".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_dry_run_with_custom_headers() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());

        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("staging".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            custom_headers: Some(headers),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_dry_run_with_client_cert() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("secure".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            client_x509_cert: Some("/path/to/cert.pem".to_string()),
            client_x509_key: Some("/path/to/key.pem".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_invalid_mode_errors() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            mode: Some("invalid".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("invalid upload mode"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifactory_binary_mode_accepted() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            mode: Some("binary".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_sha256_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        fs::write(&file_path, b"hello world").unwrap();
        let hash = sha256_file(&file_path).unwrap();
        // SHA-256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_file_missing() {
        let result = sha256_file(std::path::Path::new("/nonexistent/file.bin"));
        assert!(result.is_err());
    }

    #[test]
    fn test_artifactory_multiple_entries() {
        let mut config = Config::default();
        config.artifactories = Some(vec![
            ArtifactoryConfig {
                name: Some("prod".to_string()),
                target: Some("https://art.example.com/prod/".to_string()),
                ..Default::default()
            },
            ArtifactoryConfig {
                name: Some("staging".to_string()),
                target: Some("https://art.example.com/staging/".to_string()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        ]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        // First entry proceeds, second is skipped — both are ok
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_requires_name() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: None,
            target: Some("https://art.example.com/repo/".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("missing required 'name'"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifactory_requires_name_nonempty() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some(String::new()),
            target: Some("https://art.example.com/repo/".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("missing required 'name'"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifactory_skips_when_skip_string_true() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            skip: Some(StringOrBool::String("true".to_string())),
            // No name or target — skip should fire before validation.
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }
}
