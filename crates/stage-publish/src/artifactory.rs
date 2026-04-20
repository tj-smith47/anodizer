use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::hashing::sha256_file;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use std::fs;

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
// Artifact filtering by mode
// ---------------------------------------------------------------------------

/// Return the artifact kinds that match the given upload mode.
/// GoReleaser archive mode: archives, source archives, makeself, linux packages,
/// flatpak, python distributions.
/// GoReleaser binary mode: compiled binaries only.
fn artifact_kinds_for_mode(mode: &str) -> Vec<ArtifactKind> {
    match mode {
        "binary" => vec![ArtifactKind::UploadableBinary],
        _ => vec![
            ArtifactKind::Archive,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Flatpak,
        ],
    }
}

/// Collect artifacts matching mode, optional ID filter, and optional extension filter.
/// Also collects checksum/signature/metadata artifacts and extra files when configured.
#[allow(clippy::too_many_arguments)]
pub fn collect_upload_artifacts<'a>(
    ctx: &'a Context,
    mode: &str,
    ids: Option<&[String]>,
    exts: Option<&[String]>,
    include_checksum: bool,
    include_signature: bool,
    include_meta: bool,
    extra_files_only: bool,
) -> Vec<&'a Artifact> {
    // If extra_files_only, skip normal artifacts entirely
    if extra_files_only {
        return ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::UploadableFile)
            .collect();
    }
    let kinds = artifact_kinds_for_mode(mode);
    let mut artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            // Must match one of the mode kinds
            if !kinds.contains(&a.kind) {
                return false;
            }
            // ID filter
            if !crate::util::matches_id_filter(a, ids) {
                return false;
            }
            // Extension filter
            if let Some(ext_list) = exts
                && !ext_list.is_empty()
            {
                let name = a.name();
                if !ext_list
                    .iter()
                    .any(|ext| name.ends_with(&format!(".{}", ext)))
                {
                    return false;
                }
            }
            true
        })
        .collect();

    // Optionally include checksum artifacts
    if include_checksum {
        for a in ctx.artifacts.all() {
            if a.kind == ArtifactKind::Checksum {
                artifacts.push(a);
            }
        }
    }
    // Optionally include signature and certificate artifacts
    // GoReleaser includes Certificate alongside Signature (http.go:218)
    if include_signature {
        for a in ctx.artifacts.all() {
            if a.kind == ArtifactKind::Signature || a.kind == ArtifactKind::Certificate {
                artifacts.push(a);
            }
        }
    }
    // Optionally include metadata artifacts
    if include_meta {
        for a in ctx.artifacts.all() {
            if a.kind == ArtifactKind::Metadata {
                artifacts.push(a);
            }
        }
    }

    artifacts
}

// ---------------------------------------------------------------------------
// build_reqwest_client
// ---------------------------------------------------------------------------

/// Build a reqwest blocking client with optional mTLS and trusted CA certs.
pub fn build_reqwest_client(
    client_cert_path: Option<&str>,
    client_key_path: Option<&str>,
    trusted_certs_pem: Option<&str>,
) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::ClientBuilder::new().user_agent("anodizer/1.0");

    // mTLS client certificate
    if let (Some(cert_path), Some(key_path)) = (client_cert_path, client_key_path) {
        let cert_pem = fs::read(cert_path)
            .with_context(|| format!("artifactory: failed to read client cert '{}'", cert_path))?;
        let key_pem = fs::read(key_path)
            .with_context(|| format!("artifactory: failed to read client key '{}'", key_path))?;
        // Identity::from_pem expects a single PEM buffer with both cert and key
        let mut combined_pem = cert_pem;
        combined_pem.push(b'\n');
        combined_pem.extend_from_slice(&key_pem);
        let identity = reqwest::Identity::from_pem(&combined_pem)
            .context("artifactory: failed to load client certificate identity")?;
        builder = builder.identity(identity);
    } else if client_cert_path.is_some() != client_key_path.is_some() {
        bail!(
            "artifactory: client_x509_cert and client_x509_key must both be set (or both omitted)"
        );
    }

    // Trusted CA certificates
    if let Some(pem_data) = trusted_certs_pem {
        for cert in reqwest::Certificate::from_pem_bundle(pem_data.as_bytes())
            .context("artifactory: failed to parse trusted_certificates PEM")?
        {
            builder = builder.add_root_certificate(cert);
        }
    }

    builder
        .build()
        .context("artifactory: failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// render_artifact_url
// ---------------------------------------------------------------------------

/// Render a target URL template with artifact-specific variables.
/// Supports {{ .ProjectName }}, {{ .Version }}, {{ .Tag }}, {{ .Os }}, {{ .Arch }},
/// and {{ .ArtifactName }}. Falls back to ctx.render_template for global vars.
fn render_artifact_url(
    ctx: &Context,
    template: &str,
    artifact: &Artifact,
    custom_artifact_name: bool,
) -> Result<String> {
    // First pass: render global vars through the template engine
    let mut rendered = ctx
        .render_template(template)
        .with_context(|| "artifactory: failed to render target URL template")?;

    // Replace artifact-specific placeholders that the global renderer doesn't know
    let art_name = artifact.name();
    let os = artifact.goos().unwrap_or_default();
    let arch = artifact.goarch().unwrap_or_default();

    // GoReleaser uses .ArtifactName in URL templates
    rendered = rendered.replace("{{ .ArtifactName }}", art_name);
    rendered = rendered.replace("{{.ArtifactName}}", art_name);

    // If custom_artifact_name is false (default), append artifact name to URL
    if !custom_artifact_name {
        if !rendered.ends_with('/') {
            rendered.push('/');
        }
        rendered.push_str(art_name);
    }

    // Replace any remaining .Os / .Arch patterns
    rendered = rendered.replace("{{ .Os }}", &os);
    rendered = rendered.replace("{{.Os}}", &os);
    rendered = rendered.replace("{{ .Arch }}", &arch);
    rendered = rendered.replace("{{.Arch}}", &arch);

    Ok(rendered)
}

// ---------------------------------------------------------------------------
// upload_single_artifact
// ---------------------------------------------------------------------------

/// Upload a single artifact to the target URL.
#[allow(clippy::too_many_arguments)]
pub fn upload_single_artifact(
    client: &reqwest::blocking::Client,
    method: &str,
    url: &str,
    username: &str,
    password: &str,
    checksum_header: &str,
    custom_headers: &HashMap<String, String>,
    artifact: &Artifact,
    ctx: &Context,
    log: &StageLogger,
) -> Result<()> {
    let path = &artifact.path;
    if !path.exists() {
        bail!("artifactory: artifact file not found: {}", path.display());
    }
    if path.is_dir() {
        bail!(
            "artifactory: upload failed: the asset to upload can't be a directory: {}",
            path.display()
        );
    }

    // Compute SHA-256 checksum
    let checksum = sha256_file(path)?;

    // Read file body
    let body = fs::read(path)
        .with_context(|| format!("artifactory: failed to read '{}'", path.display()))?;

    log.status(&format!(
        "uploading {} ({} bytes) to {}",
        artifact.name(),
        body.len(),
        url
    ));

    // Build request
    let mut req = match method.to_uppercase().as_str() {
        "PUT" => client.put(url),
        "POST" => client.post(url),
        other => bail!("artifactory: unsupported HTTP method '{}'", other),
    };

    // Basic Auth
    if !username.is_empty() && !password.is_empty() {
        req = req.basic_auth(username, Some(password));
    }

    // Checksum header
    if !checksum_header.is_empty() {
        req = req.header(checksum_header, &checksum);
    }

    // Custom headers (template-rendered with artifact context)
    // GoReleaser template-renders custom_headers values with artifact-specific
    // variables (ArtifactName, Os, Arch, etc.), not just global template vars.
    for (k, v) in custom_headers {
        let rendered_v = {
            let mut vars = ctx.template_vars().clone();
            vars.set("ArtifactName", artifact.name());
            vars.set(
                "ArtifactExt",
                anodizer_core::template::extract_artifact_ext(artifact.name()),
            );
            if let Some(ref target) = artifact.target {
                let (os, arch) = anodizer_core::target::map_target(target);
                vars.set("Os", &os);
                vars.set("Arch", &arch);
                vars.set("Target", target);
            }
            anodizer_core::template::render(v, &vars).unwrap_or_else(|_| v.clone())
        };
        req = req.header(k.as_str(), rendered_v);
    }

    // Content-Length is set automatically by reqwest from body length
    req = req.header("Content-Length", body.len().to_string());

    let resp = req
        .body(body)
        .send()
        .with_context(|| format!("artifactory: HTTP request failed for '{}'", url))?;

    let status = resp.status();
    if !status.is_success() {
        let resp_body = resp.text().unwrap_or_default();
        // Try to extract JSON error details (Artifactory format)
        let detail = if let Ok(json) = serde_json::from_str::<serde_json::Value>(&resp_body) {
            if let Some(errors) = json.get("errors").and_then(|e| e.as_array()) {
                errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ")
            } else {
                resp_body
            }
        } else {
            resp_body
        };
        bail!(
            "artifactory: upload of '{}' failed: {} {} — {}",
            artifact.name(),
            method.to_uppercase(),
            status,
            detail
        );
    }

    log.status(&format!("uploaded {} ({})", artifact.name(), status));
    Ok(())
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
        // Check disable first (matches GoReleaser's Upload publisher `disable` field).
        if let Some(ref d) = entry.disable
            && d.is_disabled(|tmpl| ctx.render_template(tmpl))
        {
            let entry_name = entry.name.as_deref().unwrap_or("<unnamed>");
            log.status(&format!("artifactory: '{}' disabled", entry_name));
            continue;
        }

        // Check skip flag.
        if let Some(ref s) = entry.skip
            && s.is_disabled(|tmpl| ctx.render_template(tmpl))
        {
            log.status("artifactory: entry skipped");
            continue;
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

        // HTTP method (default: PUT).
        let method = entry.method.as_deref().unwrap_or("PUT");

        // Resolve credentials via the anodizer ctx env resolver (matches
        // GoReleaser internal/http/http.go:168-178). Cascade:
        //   Username: config → ARTIFACTORY_{NAME}_USERNAME
        //   Password: config → ARTIFACTORY_{NAME}_SECRET
        // The generic `ARTIFACTORY_SECRET` fallback is removed — it leaks one
        // instance's credentials across every configured artifactory entry,
        // which GoReleaser does not do.  Env vars are looked up through the
        // ctx env map so project `env:` / `env_files:` values are visible.
        let env_map = ctx.template_vars().all_env();
        let lookup_env = |name: &str| -> Option<String> {
            env_map
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .filter(|s| !s.is_empty())
        };
        let name_upper = name.to_uppercase().replace('-', "_");
        let username_env_var = format!("ARTIFACTORY_{}_USERNAME", name_upper);
        let username = match entry.username {
            Some(ref u) => ctx.render_template(u).with_context(|| {
                format!("artifactory: failed to render username for '{}'", name)
            })?,
            None => lookup_env(&username_env_var).unwrap_or_default(),
        };
        let named_env_var = format!("ARTIFACTORY_{}_SECRET", name_upper);
        let password = lookup_env(&named_env_var)
            .or_else(|| {
                entry
                    .password
                    .as_ref()
                    .and_then(|p| ctx.render_template(p).ok())
            })
            .unwrap_or_default();

        // Determine checksum header name (GoReleaser default: X-Checksum-SHA256).
        let checksum_header = entry
            .checksum_header
            .as_deref()
            .unwrap_or("X-Checksum-SHA256");

        // Collect custom headers.
        let empty = HashMap::new();
        let custom_headers = entry.custom_headers.as_ref().unwrap_or(&empty);

        // Include flags
        let include_checksum = entry.checksum.unwrap_or(false);
        let include_signature = entry.signature.unwrap_or(false);
        let include_meta = entry.meta.unwrap_or(false);
        let custom_artifact_name = entry.custom_artifact_name.unwrap_or(false);
        let extra_files_only = entry.extra_files_only.unwrap_or(false);

        // --- Dry-run logging ---
        if ctx.is_dry_run() {
            let target_url = ctx.render_template(target_template).with_context(|| {
                format!("artifactory: failed to render target URL for '{}'", name)
            })?;
            log.status(&format!(
                "(dry-run) would upload artifacts to Artifactory '{}' at {} (mode={}, method={}, user={})",
                name, target_url, mode, method, username
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
            if entry.trusted_certificates.is_some() {
                log.status("(dry-run) using custom trusted certificates");
            }
            log.status(&format!("(dry-run) checksum header: {}", checksum_header));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) build ID filter: {:?}", ids));
            }
            if let Some(ref exts) = entry.exts {
                log.status(&format!("(dry-run) extension filter: {:?}", exts));
            }
            if include_checksum {
                log.status("(dry-run) include checksums: true");
            }
            if include_signature {
                log.status("(dry-run) include signatures: true");
            }
            if include_meta {
                log.status("(dry-run) include metadata: true");
            }
            if custom_artifact_name {
                log.status("(dry-run) custom artifact naming: true");
            }
            if let Some(ref files) = entry.extra_files {
                log.status(&format!("(dry-run) extra files: {} entries", files.len()));
            }
            log.status(&format!("(dry-run) credential env var: {}", named_env_var));

            // Log matching artifacts in dry-run
            let artifacts = collect_upload_artifacts(
                ctx,
                mode,
                entry.ids.as_deref(),
                entry.exts.as_deref(),
                include_checksum,
                include_signature,
                include_meta,
                extra_files_only,
            );
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            for a in &artifacts {
                log.status(&format!("(dry-run)   {} ({})", a.name(), a.kind));
            }
            continue;
        }

        // --- Live mode ---

        // Cross-validate credentials: both must be set or both empty (GoReleaser CheckConfig parity)
        if !username.is_empty() && password.is_empty() {
            bail!(
                "artifactory: entry '{}' has username set but no password (set {} or config password)",
                name,
                named_env_var
            );
        }
        if !password.is_empty() && username.is_empty() {
            bail!(
                "artifactory: entry '{}' has password/secret set but no username (set username in config or {})",
                name,
                username_env_var
            );
        }

        // Validate mTLS cert/key pair
        if entry.client_x509_cert.is_some() != entry.client_x509_key.is_some() {
            bail!(
                "artifactory: entry '{}': client_x509_cert and client_x509_key must both be set",
                name
            );
        }

        // Build HTTP client
        let client = build_reqwest_client(
            entry.client_x509_cert.as_deref(),
            entry.client_x509_key.as_deref(),
            entry.trusted_certificates.as_deref(),
        )?;

        // Collect artifacts
        let artifacts = collect_upload_artifacts(
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
                "artifactory: no matching artifacts for '{}' (mode={})",
                name, mode
            ));
            continue;
        }

        log.status(&format!(
            "artifactory: uploading {} artifacts to '{}' (mode={})",
            artifacts.len(),
            name,
            mode
        ));

        // Upload each artifact
        for artifact in &artifacts {
            let url = render_artifact_url(ctx, target_template, artifact, custom_artifact_name)?;
            upload_single_artifact(
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

        log.status(&format!("artifactory: upload complete for '{}'", name));
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
    use anodizer_core::config::{ArtifactoryConfig, Config, StringOrBool};
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
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("chk".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            checksum_header: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
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
            target: None,
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
            target: Some(String::new()),
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
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_default_method_is_put() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("test".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            method: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        // Should succeed with default PUT method
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_custom_method() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("test".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            method: Some("POST".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_trusted_certificates_in_dry_run() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("test".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            trusted_certificates: Some(
                "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----".to_string(),
            ),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }

    #[test]
    fn test_artifactory_username_without_password_errors_in_live_mode() {
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("test".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            username: Some("deployer".to_string()),
            password: None,
            ..Default::default()
        }]);
        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("has username set but no password"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_artifact_kinds_for_mode_archive() {
        let kinds = artifact_kinds_for_mode("archive");
        assert!(kinds.contains(&ArtifactKind::Archive));
        assert!(kinds.contains(&ArtifactKind::SourceArchive));
        assert!(kinds.contains(&ArtifactKind::LinuxPackage));
        assert!(!kinds.contains(&ArtifactKind::UploadableBinary));
    }

    #[test]
    fn test_artifact_kinds_for_mode_binary() {
        let kinds = artifact_kinds_for_mode("binary");
        assert!(kinds.contains(&ArtifactKind::UploadableBinary));
        assert!(!kinds.contains(&ArtifactKind::Archive));
    }

    #[test]
    fn test_collect_upload_artifacts_by_mode() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());

        // Add archive artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Add binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::UploadableBinary,
            name: String::new(),
            path: PathBuf::from("dist/testapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Archive mode should find archive but not binary
        let archive_arts =
            collect_upload_artifacts(&ctx, "archive", None, None, false, false, false, false);
        assert_eq!(archive_arts.len(), 1);
        assert_eq!(archive_arts[0].kind, ArtifactKind::Archive);

        // Binary mode should find binary but not archive
        let binary_arts =
            collect_upload_artifacts(&ctx, "binary", None, None, false, false, false, false);
        assert_eq!(binary_arts.len(), 1);
        assert_eq!(binary_arts[0].kind, ArtifactKind::UploadableBinary);
    }

    #[test]
    fn test_collect_upload_artifacts_with_ext_filter() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "testapp-1.0.0.tar.gz".to_string(),
            path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "testapp-1.0.0.zip".to_string(),
            path: PathBuf::from("dist/testapp-1.0.0.zip"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let exts = vec!["zip".to_string()];
        let arts = collect_upload_artifacts(
            &ctx,
            "archive",
            None,
            Some(&exts),
            false,
            false,
            false,
            false,
        );
        assert_eq!(arts.len(), 1);
        assert!(arts[0].name().ends_with(".zip"));
    }

    #[test]
    fn test_collect_upload_artifacts_includes_checksums() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: PathBuf::from("dist/checksums.txt"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Without include_checksum
        let arts =
            collect_upload_artifacts(&ctx, "archive", None, None, false, false, false, false);
        assert_eq!(arts.len(), 1);

        // With include_checksum
        let arts = collect_upload_artifacts(&ctx, "archive", None, None, true, false, false, false);
        assert_eq!(arts.len(), 2);
    }

    #[test]
    fn test_render_artifact_url_appends_name() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        let artifact = Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp-1.0.0.tar.gz".to_string(),
            path: PathBuf::from("dist/myapp-1.0.0.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };

        // Without custom_artifact_name, appends artifact name to URL
        let url =
            render_artifact_url(&ctx, "https://art.example.com/repo", &artifact, false).unwrap();
        assert!(url.ends_with("/myapp-1.0.0.tar.gz"));

        // With custom_artifact_name, does NOT append
        let url =
            render_artifact_url(&ctx, "https://art.example.com/repo", &artifact, true).unwrap();
        assert!(!url.ends_with("/myapp-1.0.0.tar.gz"));
    }

    #[test]
    fn test_dry_run_lists_matching_artifacts() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
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
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = ctx.logger("artifactory");
        assert!(publish_to_artifactory(&ctx, &log).is_ok());
    }
}
