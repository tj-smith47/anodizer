use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;

use crate::artifactory::{self, validate_upload_mode};

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
        if let Some(ref d) = entry.disable
            && d.is_disabled(|tmpl| ctx.render_template(tmpl))
        {
            log.status("upload: entry skipped (disabled)");
            continue;
        }

        // Name defaults to "upload" for env var naming
        let name = entry.name.as_deref().unwrap_or("upload");

        // Validate mode (default: "archive")
        let mode = entry.mode.as_deref().unwrap_or("archive");
        validate_upload_mode(mode)?;

        // Target URL is required
        if entry.target.is_empty() {
            bail!("upload: entry '{}' is missing required 'target' URL", name);
        }
        let target_template = &entry.target;

        // HTTP method (default: PUT)
        let method = entry.method.as_deref().unwrap_or("PUT");

        // Resolve credentials — env var cascade:
        // Username: config → UPLOAD_{NAME}_USERNAME
        // Password: UPLOAD_{NAME}_SECRET → config
        let name_upper = name.to_uppercase().replace('-', "_");
        // Resolve UPLOAD_<NAME>_USERNAME / _SECRET via the anodize ctx env map
        // (matches GoReleaser internal/http/http.go:163-164,176-177) so project
        // `env:` / `env_files:` values are visible to the upload publisher.
        let env_map = ctx.template_vars().all_env();
        let lookup_env = |name: &str| -> Option<String> {
            env_map
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .filter(|s| !s.is_empty())
        };
        let username = entry
            .username
            .as_ref()
            .and_then(|u| ctx.render_template(u).ok())
            .unwrap_or_else(|| {
                lookup_env(&format!("UPLOAD_{}_USERNAME", name_upper)).unwrap_or_default()
            });
        let password = lookup_env(&format!("UPLOAD_{}_SECRET", name_upper))
            .or_else(|| {
                entry
                    .password
                    .as_ref()
                    .and_then(|p| ctx.render_template(p).ok())
            })
            .unwrap_or_default();

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
            log.verbose(&format!(
                "upload: no artifacts matched for '{}' (mode={})",
                name, mode
            ));
            continue;
        }

        if ctx.is_dry_run() {
            let target_url = ctx
                .render_template(target_template)
                .with_context(|| format!("upload: render target URL for '{}'", name))?;
            log.status(&format!(
                "(dry-run) would upload {} artifacts to '{}' at {} (mode={}, method={})",
                artifacts.len(),
                name,
                target_url,
                mode,
                method
            ));
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
            // Render target URL with artifact context
            let mut vars = ctx.template_vars().clone();
            vars.set("ArtifactName", &artifact.name);
            vars.set(
                "ArtifactExt",
                anodize_core::template::extract_artifact_ext(&artifact.name),
            );
            if let Some(ref target) = artifact.target {
                let (os, arch) = anodize_core::target::map_target(target);
                vars.set("Os", &os);
                vars.set("Arch", &arch);
                vars.set("Target", target);
            }

            let rendered_target = anodize_core::template::render(target_template, &vars)
                .with_context(|| {
                    format!("upload: render target URL for artifact '{}'", artifact.name)
                })?;

            // Build full URL — append artifact name unless custom_artifact_name is set
            let url = if custom_artifact_name {
                rendered_target
            } else {
                let sep = if rendered_target.ends_with('/') {
                    ""
                } else {
                    "/"
                };
                format!("{}{}{}", rendered_target, sep, artifact.name)
            };

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
    use anodize_core::config::Config;

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
