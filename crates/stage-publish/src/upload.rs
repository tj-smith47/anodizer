use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;

use crate::artifactory::{self, render_artifact_url, validate_upload_mode};

/// Publish artifacts to generic HTTP endpoints.
///
/// This is functionally identical to the Artifactory publisher but uses
/// `UPLOAD_{NAME}_USERNAME` / `UPLOAD_{NAME}_SECRET` environment variables
/// instead of the Artifactory-specific ones. It reuses the same artifact
/// collection, template rendering, and HTTP upload infrastructure.
pub fn publish_to_upload(ctx: &Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.uploads {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    for entry in entries {
        // Check disable flag
        if let Some(ref d) = entry.disable {
            let off = d
                .try_is_disabled(|tmpl| ctx.render_template(tmpl))
                .with_context(|| {
                    format!(
                        "upload: render disable template for entry '{}'",
                        entry.name.as_deref().unwrap_or("<unnamed>")
                    )
                })?;
            if off {
                log.status("upload: entry skipped (disabled)");
                continue;
            }
        }

        // `name` is the env-var prefix for credential lookup
        // (UPLOAD_<NAME>_USERNAME / _SECRET); two nameless entries would
        // share the same namespace, so refuse them.
        let name = entry
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "upload: entry is missing required 'name:' (used as the env-var \
                 prefix UPLOAD_<NAME>_USERNAME / UPLOAD_<NAME>_SECRET)"
                )
            })?;

        // Validate mode (default: "archive")
        let mode = entry.mode.as_deref().unwrap_or("archive");
        validate_upload_mode(mode)?;

        // Empty target soft-skips so a partly-filled YAML scaffold
        // doesn't break the whole release.
        if entry.target.is_empty() {
            log.status(&format!(
                "upload: entry '{}' has no 'target' URL configured, skipping",
                name
            ));
            continue;
        }
        let target_template = &entry.target;

        // HTTP method (default: PUT)
        let method = entry.method.as_deref().unwrap_or("PUT");

        // Cascade: explicit config first, then UPLOAD_<NAME>_USERNAME /
        // UPLOAD_<NAME>_SECRET via the ctx env map, so project `env:` /
        // `env_files:` values reach this publisher.
        let name_upper = name.to_uppercase().replace('-', "_");
        let env_map = ctx.template_vars().all_env();
        let lookup_env = |name: &str| -> Option<String> {
            env_map
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .filter(|s| !s.is_empty())
        };
        // An empty username/password after template render counts as
        // "not in config" and falls through to the env var, otherwise a
        // half-edited YAML would silently ship anonymous.
        let username = entry
            .username
            .as_ref()
            .and_then(|u| ctx.render_template(u).ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                lookup_env(&format!("UPLOAD_{}_USERNAME", name_upper)).unwrap_or_default()
            });
        let password = entry
            .password
            .as_ref()
            .and_then(|p| ctx.render_template(p).ok())
            .filter(|p| !p.is_empty())
            .or_else(|| lookup_env(&format!("UPLOAD_{}_SECRET", name_upper)))
            .unwrap_or_default();

        // A half-set credential pair almost always means a typo'd env
        // var or a stale leftover. Both empty is acceptable for targets
        // that accept anonymous PUT. Skipped in dry-run so config tests
        // don't need fake credentials.
        if !ctx.is_dry_run() && username.is_empty() != password.is_empty() {
            bail!(
                "upload: '{}' has only one of username/password set \
                 (set both to authenticate, or leave both empty for \
                 anonymous upload)",
                name
            );
        }

        // mTLS cert/key are a pair; reject upfront so a misconfigured
        // pair fails fast instead of after the artifact collection pass.
        if entry.client_x509_cert.is_some() != entry.client_x509_key.is_some() {
            bail!(
                "upload: '{}' has only one of client_x509_cert / client_x509_key set \
                 (set both to enable mTLS, or leave both empty)",
                name
            );
        }

        let checksum_header = entry.checksum_header.as_deref().unwrap_or("");
        let empty = HashMap::new();
        let custom_headers = entry.custom_headers.as_ref().unwrap_or(&empty);
        let include_checksum = entry.checksum.unwrap_or(false);
        let include_signature = entry.signature.unwrap_or(false);
        let include_meta = entry.meta.unwrap_or(false);
        let custom_artifact_name = entry.custom_artifact_name.unwrap_or(false);
        let extra_files_only = entry.extra_files_only.unwrap_or(false);

        // Collect matching artifacts
        let artifacts = artifactory::collect_upload_artifacts(
            ctx,
            mode,
            entry.ids.as_deref(),
            entry.exts.as_deref(),
            include_checksum,
            include_signature,
            include_meta,
            extra_files_only,
        );

        if artifacts.is_empty() {
            log.status(&format!(
                "upload: no artifacts matched for '{}' (mode={})",
                name, mode
            ));
            continue;
        }

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would upload {} artifacts to '{}' (mode={}, method={})",
                artifacts.len(),
                name,
                mode,
                method
            ));
            for artifact in &artifacts {
                let url =
                    render_artifact_url(ctx, target_template, artifact, custom_artifact_name)?;
                log.status(&format!(
                    "(dry-run)   {} ({}) -> {}",
                    artifact.name, artifact.kind, url
                ));
            }
            continue;
        }

        log.status(&format!(
            "uploading {} artifacts to '{}' (mode={}, method={})",
            artifacts.len(),
            name,
            mode,
            method
        ));

        // Build HTTP client (supports mTLS)
        let client = artifactory::build_reqwest_client(
            entry.client_x509_cert.as_deref(),
            entry.client_x509_key.as_deref(),
            entry.trusted_certificates.as_deref(),
        )?;

        for artifact in &artifacts {
            let url = render_artifact_url(ctx, target_template, artifact, custom_artifact_name)?;

            log.status(&format!("  {} {} -> {}", method, artifact.name, url));

            // Upload the artifact
            artifactory::upload_single_artifact(
                &client,
                method,
                &url,
                &username,
                &password,
                checksum_header,
                custom_headers,
                artifact,
                ctx,
                log,
            )?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use anodizer_core::config::Config;

    #[test]
    fn test_upload_config_parsing() {
        let yaml = r#"
project_name: test
uploads:
  - name: myserver
    target: "https://files.example.com/{{ .ProjectName }}/{{ .Version }}/"
    method: PUT
    username: deploy
    checksum_header: X-SHA256
    custom_headers:
      X-Deploy: "{{ .Tag }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let uploads = config.uploads.as_ref().unwrap();
        assert_eq!(uploads.len(), 1);
        let u = &uploads[0];
        assert_eq!(u.name.as_deref(), Some("myserver"));
        assert!(u.target.contains("example.com"));
        assert_eq!(u.method.as_deref(), Some("PUT"));
        assert_eq!(u.username.as_deref(), Some("deploy"));
        assert_eq!(u.checksum_header.as_deref(), Some("X-SHA256"));
        assert!(u.custom_headers.as_ref().unwrap().contains_key("X-Deploy"));
    }

    #[test]
    fn test_upload_config_defaults() {
        let yaml = r#"
project_name: test
uploads:
  - target: "https://example.com/upload/"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let uploads = config.uploads.as_ref().unwrap();
        let u = &uploads[0];
        // name defaults to None (will be "upload" at runtime)
        assert!(u.name.is_none());
        // method defaults to None (will be "PUT" at runtime)
        assert!(u.method.is_none());
        // mode defaults to None (will be "archive" at runtime)
        assert!(u.mode.is_none());
    }
}
