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
            if (a.kind == ArtifactKind::Signature || a.kind == ArtifactKind::Certificate)
                && !anodizer_core::artifact::is_binary_sign_output(a)
            {
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
// collect_artifactory_targets — evidence helper
// ---------------------------------------------------------------------------

/// One uploaded URL plus the name of the entry that produced it. Threaded
/// into [`PublishEvidence::extra`] so the rollback path can resolve the
/// same credentials [`publish_to_artifactory`] used (basic auth via
/// `username` + `password`, plus per-entry `ARTIFACTORY_<NAME>_SECRET`
/// overrides) rather than narrowly looking up `ARTIFACTORY_TOKEN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactoryTarget {
    pub entry: String,
    pub url: String,
}

/// Re-walk the configured artifactory entries to produce the list of fully
/// rendered upload URLs that [`publish_to_artifactory`] would PUT to. Used by
/// the [`Publisher`] wrapper to populate
/// [`anodizer_core::PublishEvidence::artifact_paths`] (URLs) and
/// [`anodizer_core::PublishEvidence::extra`] (entry-name tags) so a
/// subsequent rollback can DELETE each URL using the same credential
/// resolution the publish path used.
///
/// Best-effort: entries that hit a render or filter error are silently
/// skipped, since failures here only narrow the rollback checklist (the
/// publish path's own error handling has already surfaced any blocker).
pub(crate) fn collect_artifactory_targets(ctx: &Context) -> Vec<ArtifactoryTarget> {
    let mut out: Vec<ArtifactoryTarget> = Vec::new();
    let entries = match ctx.config.artifactories.as_ref() {
        Some(v) if !v.is_empty() => v,
        _ => return out,
    };
    for entry in entries {
        // Skip evaluation must match publish_to_artifactory's behaviour so
        // a skipped entry doesn't leak phantom rollback targets.
        if let Some(ref s) = entry.skip
            && s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .unwrap_or(false)
        {
            continue;
        }
        let entry_name = match entry.name.as_deref() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        let target_template = match entry.target.as_deref() {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        let mode = entry.mode.as_deref().unwrap_or("archive");
        let include_checksum = entry.checksum.unwrap_or(false);
        let include_signature = entry.signature.unwrap_or(false);
        let include_meta = entry.meta.unwrap_or(false);
        let custom_artifact_name = entry.custom_artifact_name.unwrap_or(false);
        let extra_files_only = entry.extra_files_only.unwrap_or(false);
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
        for a in &artifacts {
            if let Ok(url) = render_artifact_url(ctx, target_template, a, custom_artifact_name) {
                out.push(ArtifactoryTarget {
                    entry: entry_name.clone(),
                    url,
                });
            }
        }
    }
    out
}

/// Encode the per-target `(entry, url)` pairs into the JSON shape stored
/// at `PublishEvidence::extra.artifactory_targets`. The wrapper key keeps
/// the slot extensible — other publishers can land alongside without
/// colliding.
pub(crate) fn encode_artifactory_targets(targets: &[ArtifactoryTarget]) -> serde_json::Value {
    serde_json::json!({
        "artifactory_targets": targets
            .iter()
            .map(|t| serde_json::json!({ "entry": t.entry, "url": t.url }))
            .collect::<Vec<_>>(),
    })
}

/// Decode the JSON shape produced by [`encode_artifactory_targets`] back
/// into structured targets. Returns an empty vec if the field is missing
/// or malformed — rollback then falls back to URL-only deletion against
/// the legacy `ARTIFACTORY_TOKEN` ladder.
pub(crate) fn decode_artifactory_targets(extra: &serde_json::Value) -> Vec<ArtifactoryTarget> {
    let array = extra
        .get("artifactory_targets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    array
        .into_iter()
        .filter_map(|v| {
            let entry = v.get("entry")?.as_str()?.to_string();
            let url = v.get("url")?.as_str()?.to_string();
            Some(ArtifactoryTarget { entry, url })
        })
        .collect()
}

/// Resolve `(username, password)` for an artifactory entry at rollback
/// time, mirroring the exact credential cascade `publish_to_artifactory`
/// uses (config → `ARTIFACTORY_<NAME>_USERNAME` / `ARTIFACTORY_<NAME>_SECRET`
/// env, with the per-entry override honoured). Returns `None` when the
/// entry is no longer present in config (e.g. the operator pruned the
/// YAML between publish and rollback) so the caller can decide between
/// best-effort token fallback and skipping.
fn resolve_rollback_credentials(ctx: &Context, entry_name: &str) -> Option<(String, String)> {
    let entries = ctx.config.artifactories.as_ref()?;
    let entry = entries
        .iter()
        .find(|e| e.name.as_deref() == Some(entry_name))?;
    crate::http_upload::resolve_http_credentials(
        ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "artifactory",
            entry_name,
            config_username: entry.username.as_deref(),
            config_password: entry.password.as_deref(),
            env_prefix: "ARTIFACTORY",
            // Rollback is best-effort; tolerate anonymous so we surface a
            // 401 in the deletion summary rather than bailing here.
            anonymous_ok: true,
        },
    )
    .ok()
}

/// Outcome of one DELETE attempt against a single artifactory URL.
/// Returned by [`delete_one_artifactory_target`] so the per-URL response
/// can be aggregated into the summary line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeleteOutcome {
    Deleted,
    AlreadyAbsent,
    Failed(String),
}

/// Classify a DELETE response's status code into the rollback summary
/// bucket. 2xx → `Deleted`, 404 / 410 → `AlreadyAbsent`, everything else
/// → `Failed`. Pure helper so the bucket boundary can be unit-tested
/// without firing an HTTP request.
pub(crate) fn classify_delete_status(status: reqwest::StatusCode) -> DeleteOutcome {
    if status.is_success() {
        DeleteOutcome::Deleted
    } else if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        DeleteOutcome::AlreadyAbsent
    } else {
        DeleteOutcome::Failed(format!("HTTP {}", status))
    }
}

// ---------------------------------------------------------------------------
// ArtifactoryPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_artifactory`] in the [`anodizer_core::Publisher`] trait
// so the new dispatch path (see [`crate::registry::configured_publishers`])
// can drive Artifactory uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable bytes,
// server-side deletable). `required = false`.
//
// Rollback shape: per uploaded URL, issue an HTTP DELETE with the same
// credential cascade `publish_to_artifactory` uses (basic auth from
// `username` + `password` plus per-entry `ARTIFACTORY_<NAME>_SECRET`
// override; the legacy `ARTIFACTORY_TOKEN` bearer is a last-resort
// fallback when no entry name was threaded through evidence). DELETEs
// fan out under a fixed concurrency cap (4) so a v0.2.0-sized 143-artifact
// rollback finishes in minutes, not over an hour. 404 / 410 responses are
// classified `AlreadyAbsent` (not `Failed`) so a re-run after a partial
// rollback doesn't print phantom failures. The rollback function returns
// Ok regardless of per-target outcome — the summary line + per-failure
// warns carry the operator-facing diagnosis.
simple_publisher!(
    ArtifactoryPublisher,
    "artifactory",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("ARTIFACTORY_TOKEN delete"),
);

// Bound for parallel DELETE fan-out during rollback is shared with the
// Bundle B git-revert publishers via [`crate::util::ROLLBACK_PARALLELISM`].
// Re-imported below so the local references in `parallel_delete` stay
// terse.
use crate::util::ROLLBACK_PARALLELISM;

impl anodizer_core::Publisher for ArtifactoryPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::PUBLISHER_REQUIRED
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        publish_to_artifactory(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("artifactory");
        let targets = collect_artifactory_targets(ctx);
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(first.url.clone());
        }
        evidence.artifact_paths = targets
            .iter()
            .map(|t| std::path::PathBuf::from(&t.url))
            .collect();
        evidence.extra = encode_artifactory_targets(&targets);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        if evidence.artifact_paths.is_empty() && evidence.primary_ref.is_none() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "artifactory",
                "upload URLs",
            ));
            return Ok(());
        }
        // Decode the structured (entry, url) pairs from evidence.extra so
        // each DELETE can resolve credentials through the publish path's
        // own resolver (basic auth + per-entry env override). When the
        // field is missing (older evidence, or a config change between
        // publish and rollback) fall back to URL-only deletion against
        // the legacy bearer ladder so existing rollbacks don't silently
        // break.
        let structured = decode_artifactory_targets(&evidence.extra);
        let token_env = std::env::var("ARTIFACTORY_TOKEN")
            .or_else(|_| std::env::var("ARTIFACTORY_SECRET"))
            .ok();
        let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "artifactory: failed to build HTTP client for rollback: {}; manual cleanup required",
                    e
                ));
                return Ok(());
            }
        };

        // Build (url, auth) pairs honouring structured evidence first,
        // falling back to URL-only deletion against the bearer ladder for
        // legacy / pruned-config rollbacks.
        let by_url: std::collections::HashMap<String, String> = structured
            .iter()
            .map(|t| (t.url.clone(), t.entry.clone()))
            .collect();
        let jobs: Vec<RollbackJob> = evidence
            .artifact_paths
            .iter()
            .map(|p| {
                let url = p.display().to_string();
                let basic_auth = by_url
                    .get(&url)
                    .and_then(|entry| resolve_rollback_credentials(ctx, entry))
                    .filter(|(u, p)| !u.is_empty() && !p.is_empty());
                let bearer = if basic_auth.is_none() {
                    token_env.clone()
                } else {
                    None
                };
                RollbackJob {
                    url,
                    basic_auth,
                    bearer,
                }
            })
            .collect();

        let (deleted, already_absent, failed) = parallel_delete(&client, &jobs, &log);
        log.status(&format!(
            "artifactory: deleted {} artifact(s), {} already absent, {} failure(s)",
            deleted, already_absent, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

/// One rollback DELETE job: target URL + the auth to send with it.
/// `basic_auth` carries (username, password) when the entry tag in
/// `PublishEvidence::extra` resolved to a configured basic-auth pair;
/// otherwise `bearer` falls back to `ARTIFACTORY_TOKEN` /
/// `ARTIFACTORY_SECRET`. Both `None` is acceptable — the DELETE will
/// surface a 401 in the failed bucket rather than silently 200ing.
#[derive(Clone, Debug)]
struct RollbackJob {
    url: String,
    basic_auth: Option<(String, String)>,
    bearer: Option<String>,
}

/// Fan out per-URL DELETE requests under [`ROLLBACK_PARALLELISM`], applying
/// the resolved auth per request. Each request's outcome is classified via
/// [`classify_delete_status`] so 404 / 410 land in `already_absent` instead
/// of `failed`. Returns `(deleted, already_absent, failed)` counts.
fn parallel_delete(
    client: &reqwest::blocking::Client,
    jobs: &[RollbackJob],
    log: &StageLogger,
) -> (usize, usize, usize) {
    use std::sync::Mutex;
    let counts = Mutex::new((0usize, 0usize, 0usize));
    let chunks = jobs.chunks(ROLLBACK_PARALLELISM);
    for chunk in chunks {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk.len());
            for job in chunk {
                let client = client.clone();
                let url = job.url.clone();
                let basic_auth = job.basic_auth.clone();
                let bearer = job.bearer.clone();
                let log = log.clone();
                let counts = &counts;
                handles.push(s.spawn(move || {
                    log.status(&format!("artifactory: DELETE {}", url));
                    let mut req = client.delete(&url);
                    if let Some((ref u, ref p)) = basic_auth {
                        req = req.basic_auth(u, Some(p));
                    } else if let Some(ref tok) = bearer {
                        req = req.bearer_auth(tok);
                    }
                    match req.send() {
                        Ok(resp) => {
                            let status = resp.status();
                            match classify_delete_status(status) {
                                DeleteOutcome::Deleted => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.0 += 1;
                                }
                                DeleteOutcome::AlreadyAbsent => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.1 += 1;
                                    log.status(&format!(
                                        "artifactory: DELETE {} returned HTTP {} (already absent)",
                                        url, status
                                    ));
                                }
                                DeleteOutcome::Failed(_) => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.2 += 1;
                                    log.warn(&format!(
                                        "artifactory: DELETE {} returned HTTP {} (manual cleanup may be required)",
                                        url, status
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                            c.2 += 1;
                            log.warn(&format!(
                                "artifactory: DELETE {} transport error: {} (manual cleanup may be required)",
                                url, e
                            ));
                        }
                    }
                }));
            }
            for h in handles {
                crate::util::join_or_warn(h, log, "artifactory");
            }
        });
    }
    // `into_inner` consumes the Mutex; poison here means a worker
    // panicked. Counter state is still valid (3-tuple of usize) so
    // recover and emit the summary rather than abandon the operator.
    match counts.into_inner() {
        Ok(c) => c,
        Err(poisoned) => {
            log.warn(
                "artifactory: mutex poisoned by worker panic; reporting counters as-of poison",
            );
            poisoned.into_inner()
        }
    }
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn artifactory_publisher_classification() {
        let p = ArtifactoryPublisher::new();
        assert_eq!(p.name(), "artifactory");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(p.rollback_scope_needed(), Some("ARTIFACTORY_TOKEN delete"));
    }

    #[test]
    fn artifactory_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = ArtifactoryPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn artifactory_rollback_warns_when_no_targets_recorded() {
        // Empty evidence — rollback emits a single warn and returns Ok.
        // The warn text is fixed by `rollback_empty_warning_msg` and
        // independently asserted there; this case proves the empty branch
        // does not crash and returns Ok.
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("artifactory");
        let p = ArtifactoryPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
    }

    /// The empty-evidence warn text comes from the shared helper. Tests
    /// across the four Bundle A publishers reuse this helper so the
    /// message wording can be pinned in one place.
    #[test]
    fn artifactory_rollback_empty_warning_msg_shape() {
        let msg =
            crate::publisher_helpers::rollback_empty_warning_msg("artifactory", "upload URLs");
        assert!(msg.starts_with("artifactory:"), "{msg}");
        assert!(msg.contains("upload URLs"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    /// Critical #1 — rollback must reuse the publish path's basic-auth
    /// credentials, not narrowly read `ARTIFACTORY_TOKEN`. Verified at
    /// the seam: the helper that resolves a given entry's credentials
    /// returns the configured (username, password) for an entry whose
    /// config carries them.
    #[test]
    fn artifactory_rollback_uses_publish_credentials() {
        use anodizer_core::config::{ArtifactoryConfig, Config};
        use anodizer_core::context::ContextOptions;
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            username: Some("deployer".to_string()),
            password: Some("hunter2".to_string()),
            ..Default::default()
        }]);
        let ctx = Context::new(config, ContextOptions::default());
        let resolved = resolve_rollback_credentials(&ctx, "prod")
            .expect("entry credentials must resolve via publish-path helper");
        assert_eq!(resolved.0, "deployer");
        assert_eq!(resolved.1, "hunter2");
    }

    /// Critical #3 — 404 / 410 on DELETE classify as already-absent so a
    /// re-run after a partial rollback does not print false failures.
    #[test]
    fn artifactory_rollback_treats_404_as_already_absent() {
        let outcome = classify_delete_status(reqwest::StatusCode::NOT_FOUND);
        assert!(matches!(outcome, DeleteOutcome::AlreadyAbsent));
        let outcome = classify_delete_status(reqwest::StatusCode::GONE);
        assert!(matches!(outcome, DeleteOutcome::AlreadyAbsent));
    }

    /// 2xx → Deleted; everything else → Failed (so 5xx still surfaces as
    /// a failure for the operator).
    #[test]
    fn artifactory_rollback_classifies_status_buckets() {
        assert!(matches!(
            classify_delete_status(reqwest::StatusCode::OK),
            DeleteOutcome::Deleted
        ));
        assert!(matches!(
            classify_delete_status(reqwest::StatusCode::NO_CONTENT),
            DeleteOutcome::Deleted
        ));
        assert!(matches!(
            classify_delete_status(reqwest::StatusCode::UNAUTHORIZED),
            DeleteOutcome::Failed(_)
        ));
        assert!(matches!(
            classify_delete_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            DeleteOutcome::Failed(_)
        ));
    }

    /// Round-trip the structured (entry, url) JSON shape so a future
    /// schema change cannot silently break rollback's entry lookup.
    #[test]
    fn artifactory_rollback_target_extra_roundtrips() {
        let targets = vec![
            ArtifactoryTarget {
                entry: "prod".to_string(),
                url: "https://art.example.com/repo/foo.tar.gz".to_string(),
            },
            ArtifactoryTarget {
                entry: "staging".to_string(),
                url: "https://art.example.com/staging/bar.zip".to_string(),
            },
        ];
        let encoded = encode_artifactory_targets(&targets);
        let decoded = decode_artifactory_targets(&encoded);
        assert_eq!(decoded, targets);
    }

    /// A missing or malformed `artifactory_targets` field decodes to an
    /// empty vec so rollback falls back to URL-only deletion without
    /// panicking.
    #[test]
    fn artifactory_rollback_target_extra_tolerates_missing_field() {
        assert!(decode_artifactory_targets(&serde_json::Value::Null).is_empty());
        assert!(decode_artifactory_targets(&serde_json::json!({})).is_empty());
        assert!(
            decode_artifactory_targets(&serde_json::json!({ "artifactory_targets": "bogus" }))
                .is_empty()
        );
    }
}
