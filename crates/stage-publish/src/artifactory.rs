use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::hashing::sha256_file;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use std::fs;

// ---------------------------------------------------------------------------
// validate_upload_mode
// ---------------------------------------------------------------------------

/// Validate the upload mode string. Only `"archive"` and `"binary"` are
/// accepted; matching is case-insensitive so `mode: Archive` works.
pub fn validate_upload_mode(mode: &str) -> Result<()> {
    match mode.to_ascii_lowercase().as_str() {
        "archive" | "binary" => Ok(()),
        _ => bail!(
            "artifactory: invalid upload mode '{}' (expected 'archive' or 'binary')",
            mode
        ),
    }
}

// ---------------------------------------------------------------------------
// Artifact filtering by mode
// ---------------------------------------------------------------------------

/// Return the artifact kinds that match the given upload mode.
/// `binary` selects compiled binaries; everything else selects every
/// uploadable artifact kind.
fn artifact_kinds_for_mode(mode: &str) -> Vec<ArtifactKind> {
    match mode.to_ascii_lowercase().as_str() {
        "binary" => vec![ArtifactKind::UploadableBinary],
        _ => vec![
            ArtifactKind::Archive,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Flatpak,
            ArtifactKind::SourceRpm,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
        ],
    }
}

/// Bundling flags for [`collect_upload_artifacts`].
///
/// Each `bool` toggles inclusion of an extra artifact category alongside
/// the mode-selected primary artifacts. `extra_files_only` short-circuits
/// the entire selection — when set, only [`ArtifactKind::UploadableFile`]
/// items are returned and the other flags are ignored.
#[derive(Clone, Copy, Default)]
pub struct CollectFlags {
    pub checksum: bool,
    pub signature: bool,
    pub meta: bool,
    pub extra_files_only: bool,
}

/// Collect artifacts matching mode, optional ID filter, and optional extension filter.
/// Also collects checksum/signature/metadata artifacts and extra files when configured.
pub fn collect_upload_artifacts<'a>(
    ctx: &'a Context,
    mode: &str,
    ids: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
) -> Vec<&'a Artifact> {
    let CollectFlags {
        checksum: include_checksum,
        signature: include_signature,
        meta: include_meta,
        extra_files_only,
    } = flags;
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

    // Trusted CA certificates. A set-but-empty bundle almost always means
    // a copy-paste accident (PEM headers stripped, base64 truncated); bail
    // with a clear message instead of installing an empty trust store.
    if let Some(pem_data) = trusted_certs_pem {
        let trimmed = pem_data.trim();
        if trimmed.is_empty() {
            bail!(
                "artifactory: trusted_certificates is set but empty (remove the field \
                 to use the system trust store, or supply a valid PEM bundle)"
            );
        }
        let certs = reqwest::Certificate::from_pem_bundle(pem_data.as_bytes())
            .context("artifactory: failed to parse trusted_certificates PEM")?;
        if certs.is_empty() {
            bail!(
                "artifactory: trusted_certificates contains no parseable certificates \
                 (check PEM headers and that the bundle is not truncated)"
            );
        }
        for cert in certs {
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

/// Render a target URL template with the artifact context bound
/// (Os, Arch, Target, ArtifactName, ArtifactExt).
///
/// When `custom_artifact_name` is false and the template does not already
/// reference `ArtifactName`, the artifact name is appended after the
/// rendered URL — guarding against the `…/foo.tar.gz/foo.tar.gz`
/// double-name when a user writes `target: ".../{{ .ArtifactName }}"`.
pub fn render_artifact_url(
    ctx: &Context,
    template: &str,
    artifact: &Artifact,
    custom_artifact_name: bool,
) -> Result<String> {
    let mut vars = ctx.template_vars().clone();
    let art_name = artifact.name();
    vars.set("ArtifactName", art_name);
    vars.set(
        "ArtifactExt",
        anodizer_core::template::extract_artifact_ext(art_name),
    );
    if let Some(ref target) = artifact.target {
        let (os, arch) = anodizer_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    }

    let mut rendered = anodizer_core::template::render(template, &vars)
        .with_context(|| "artifactory: failed to render target URL template")?;

    // The substring check matches both `ArtifactName` and `.ArtifactName`
    // so the same guard works for Tera and Go-template syntax.
    if !custom_artifact_name && !template.contains("ArtifactName") {
        if !rendered.ends_with('/') {
            rendered.push('/');
        }
        rendered.push_str(art_name);
    }

    Ok(rendered)
}

// ---------------------------------------------------------------------------
// upload_single_artifact
// ---------------------------------------------------------------------------

/// HTTP request descriptor for [`upload_single_artifact`].
///
/// Bundles the four "what URL / how to address it" fields. The
/// `checksum_header` slot, when non-empty, names a custom HTTP header
/// (e.g. `X-Checksum-Sha256`) that is set to the artifact's hex SHA-256
/// before the request is dispatched.
#[derive(Clone, Copy)]
pub struct UploadHeaders<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub checksum_header: &'a str,
    pub custom_headers: &'a HashMap<String, String>,
}

/// HTTP basic-auth credentials for [`upload_single_artifact`]. Either both
/// fields are non-empty (auth applied) or both are empty (anonymous).
#[derive(Clone, Copy)]
pub struct UploadAuth<'a> {
    pub username: &'a str,
    pub password: &'a str,
}

/// Upload a single artifact to the target URL.
///
/// Drives the per-attempt request through [`retry_http_blocking`], which
/// applies the shared `retry_sync` machinery: transport errors, 5xx
/// responses, and 429s retry per the user's `retry:` config (mirrors
/// GoReleaser `internal/pipe/upload/upload.go::doUpload`); 4xx responses
/// fast-fail.
pub fn upload_single_artifact(
    client: &reqwest::blocking::Client,
    headers: &UploadHeaders<'_>,
    auth: &UploadAuth<'_>,
    artifact: &Artifact,
    ctx: &Context,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<()> {
    let UploadHeaders {
        method,
        url,
        checksum_header,
        custom_headers,
    } = *headers;
    let UploadAuth { username, password } = *auth;
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

    // Pre-compute rendered custom-header values so we don't re-render on
    // every retry attempt (and so render failures fail-fast outside the
    // retry loop, where they belong).
    //
    // A render failure here surfaces as a configuration error (bad template
    // syntax, missing variable, …); silently keeping the unrendered value
    // would push `{{ ... }}` literals onto the wire as header values, which
    // Artifactory typically rejects with a confusing 400. Honest fail-fast
    // matches what the comment above promises.
    let mut rendered_headers: Vec<(String, String)> = Vec::with_capacity(custom_headers.len());
    for (k, v) in custom_headers {
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
        let rendered_v = anodizer_core::template::render(v, &vars).with_context(|| {
            format!(
                "artifactory: rendering custom header '{}' for '{}'",
                k,
                artifact.name()
            )
        })?;
        rendered_headers.push((k.clone(), rendered_v));
    }

    // Validate the HTTP method up-front so the per-attempt send closure
    // can't see an unsupported value (and so a typo fails-fast outside
    // the retry loop, where it belongs — rebuilding the same Break error
    // on every attempt is wasted work).
    let method_upper = method.to_uppercase();
    match method_upper.as_str() {
        "PUT" | "POST" => {}
        other => bail!("artifactory: unsupported HTTP method '{}'", other),
    }

    let label = format!("artifactory: upload of '{}'", artifact.name());
    let art_name = artifact.name().to_string();
    let (status, _body) = retry_http_blocking(
        &label,
        policy,
        SuccessClass::AllowRedirects,
        |attempt| {
            if attempt > 1 {
                log.verbose(&format!(
                    "artifactory: retrying upload of {art_name} (attempt {attempt})"
                ));
            }
            let mut req = match method_upper.as_str() {
                "PUT" => client.put(url),
                // Validated above; the only other accepted value.
                _ => client.post(url),
            };
            if !username.is_empty() && !password.is_empty() {
                req = req.basic_auth(username, Some(password));
            }
            if !checksum_header.is_empty() {
                req = req.header(checksum_header, &checksum);
            }
            for (k, v) in &rendered_headers {
                req = req.header(k.as_str(), v);
            }
            req = req.header("Content-Length", body.len().to_string());
            req.body(body.clone()).send()
        },
        |status, resp_body| {
            // Decode Artifactory's `{"errors":[{...}]}` envelope so the
            // error message carries upstream status + message; the helper
            // wraps this in HttpError so is_retriable routes 5xx/429 to
            // retry and 4xx to fast-fail.
            let detail = decode_artifactory_error_body(resp_body);
            format!(
                "artifactory: upload of '{art_name}' failed: {method_upper} {status} — {detail}"
            )
        },
    )?;

    log.status(&format!("uploaded {} ({})", artifact.name(), status));
    Ok(())
}

/// Decode Artifactory's `{"errors":[{"status":N,"message":"..."}]}` error
/// envelope into a human-readable string. Falls back to the raw body when
/// JSON decoding fails or the envelope shape doesn't match.
fn decode_artifactory_error_body(body: &str) -> String {
    // Defense-in-depth: if Artifactory echoes our Authorization header back
    // in the error envelope, scrub the token before it lands in the
    // user-visible log. Applied at the fallback / joined-output boundary so
    // we redact once regardless of which path produces the message.
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return redact_bearer_tokens(body);
    };
    let Some(errors) = json.get("errors").and_then(|e| e.as_array()) else {
        return redact_bearer_tokens(body);
    };
    let joined: String = errors
        .iter()
        .map(|e| {
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            match e.get("status") {
                Some(s) if !s.is_null() => {
                    let s_str = s
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| s.as_i64().map(|n| n.to_string()))
                        .unwrap_or_else(|| s.to_string());
                    if msg.is_empty() {
                        format!("status={}", s_str)
                    } else {
                        format!("status={} {}", s_str, msg)
                    }
                }
                _ => msg.to_string(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        redact_bearer_tokens(body)
    } else {
        redact_bearer_tokens(&joined)
    }
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

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every entry's per-artifact upload (mirrors GoReleaser, where the
    // `retryx` policy is captured once per pipe invocation).
    let policy = ctx.retry_policy();

    for entry in entries {
        // Check skip flag.
        if let Some(ref s) = entry.skip {
            let off = s
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| {
                    format!(
                        "artifactory: render skip template for entry '{}'",
                        entry.name.as_deref().unwrap_or("<unnamed>")
                    )
                })?;
            if off {
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

        // HTTP method (default: PUT).
        let method = entry.method.as_deref().unwrap_or("PUT");

        // Credential cascade lives in http_upload::resolve_http_credentials
        // so artifactory + upload share one implementation. Refuses
        // anonymous (anonymous_ok=false) since artifactory always requires
        // creds.
        let (username, password) = crate::http_upload::resolve_http_credentials(
            ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "artifactory",
                entry_name: name,
                config_username: entry.username.as_deref(),
                config_password: entry.password.as_deref(),
                env_prefix: "ARTIFACTORY",
                anonymous_ok: false,
            },
        )?;
        let name_upper = name.to_uppercase().replace('-', "_");
        let named_env_var = format!("ARTIFACTORY_{}_SECRET", name_upper);

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
                CollectFlags {
                    checksum: include_checksum,
                    signature: include_signature,
                    meta: include_meta,
                    extra_files_only,
                },
            );
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            // Render per-artifact URLs through the same path live mode uses
            // so dry-run reflects template behaviour exactly.
            for a in &artifacts {
                let url = render_artifact_url(ctx, target_template, a, custom_artifact_name)?;
                log.status(&format!("(dry-run)   {} ({}) -> {}", a.name(), a.kind, url));
            }
            continue;
        }

        // --- Live mode ---
        //
        // Credentials are already validated above; live mode just needs
        // mTLS pair coherence.
        crate::http_upload::validate_mtls_pair(
            "artifactory",
            name,
            entry.client_x509_cert.as_deref(),
            entry.client_x509_key.as_deref(),
        )?;

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
            CollectFlags {
                checksum: include_checksum,
                signature: include_signature,
                meta: include_meta,
                extra_files_only,
            },
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
                &UploadHeaders {
                    method,
                    url: &url,
                    checksum_header,
                    custom_headers,
                },
                &UploadAuth {
                    username: &username,
                    password: &password,
                },
                artifact,
                ctx,
                &policy,
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
            collect_upload_artifacts(&ctx, "archive", None, None, CollectFlags::default());
        assert_eq!(archive_arts.len(), 1);
        assert_eq!(archive_arts[0].kind, ArtifactKind::Archive);

        // Binary mode should find binary but not archive
        let binary_arts =
            collect_upload_artifacts(&ctx, "binary", None, None, CollectFlags::default());
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
        let arts =
            collect_upload_artifacts(&ctx, "archive", None, Some(&exts), CollectFlags::default());
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
        let arts = collect_upload_artifacts(&ctx, "archive", None, None, CollectFlags::default());
        assert_eq!(arts.len(), 1);

        // With include_checksum
        let arts = collect_upload_artifacts(
            &ctx,
            "archive",
            None,
            None,
            CollectFlags {
                checksum: true,
                ..CollectFlags::default()
            },
        );
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

    /// Defense-in-depth: an Artifactory response body that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. The decode helper sits on the
    /// JSON-parse fallback, raw-body fallback, and joined-output paths;
    /// this test pins all three.
    #[test]
    fn decode_artifactory_error_body_redacts_bearer_tokens() {
        // Path 1: raw body when JSON parsing fails entirely.
        let raw = "plain-text error: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg leaked";
        let out = decode_artifactory_error_body(raw);
        assert!(
            !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "raw fallback: {out}"
        );
        assert!(
            out.contains("<redacted>"),
            "raw fallback should contain redaction marker: {out}"
        );

        // Path 2: JSON without the expected `errors` envelope.
        let no_errors = r#"{"trace":"Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
        let out = decode_artifactory_error_body(no_errors);
        assert!(
            !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "no-errors fallback: {out}"
        );
        assert!(
            out.contains("<redacted>"),
            "no-errors fallback should contain redaction marker: {out}"
        );

        // Path 3: well-formed envelope where the message itself echoes the
        // bearer token (the realistic Artifactory misbehaviour).
        let envelope = r#"{"errors":[{"status":401,"message":"bad header Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}]}"#;
        let out = decode_artifactory_error_body(envelope);
        assert!(
            !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "joined path: {out}"
        );
        assert!(
            out.contains("<redacted>"),
            "joined path should contain redaction marker: {out}"
        );
        // The non-secret prefix of the message is preserved so debugging
        // doesn't lose the upstream-supplied context.
        assert!(
            out.contains("status=401"),
            "status should survive redaction: {out}"
        );
    }
}
