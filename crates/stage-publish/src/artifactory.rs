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
pub(crate) struct CollectFlags {
    pub(crate) checksum: bool,
    pub(crate) signature: bool,
    pub(crate) meta: bool,
    pub(crate) extra_files_only: bool,
}

/// Collect artifacts matching mode, optional ID filter, and optional extension filter.
/// Also collects checksum/signature/metadata artifacts and extra files when configured.
pub(crate) fn collect_upload_artifacts<'a>(
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
            // Extension filter (case-folding via the shared matcher).
            if let Some(ext_list) = exts
                && !ext_list.is_empty()
                && !crate::util::format_matches(a.name(), ext_list)
            {
                return false;
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
    // Certificate is included alongside Signature.
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
    vars.set("ArtifactExt", &artifact.ext());
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
pub(crate) struct UploadHeaders<'a> {
    pub(crate) method: &'a str,
    pub(crate) url: &'a str,
    pub(crate) checksum_header: &'a str,
    pub(crate) custom_headers: &'a HashMap<String, String>,
}

/// HTTP basic-auth credentials for [`upload_single_artifact`]. Either both
/// fields are non-empty (auth applied) or both are empty (anonymous).
#[derive(Clone, Copy)]
pub(crate) struct UploadAuth<'a> {
    pub(crate) username: &'a str,
    pub(crate) password: &'a str,
}

/// Whether the target path already holds an artifact, and (when present)
/// whether its stored SHA-256 matches what we are about to upload.
///
/// The tri-state mirrors cargo's `is_already_published` and chocolatey's
/// `FeedHashResult`: the `Unknown` arm exists so a probe that can't prove
/// either presence or content-match never causes a false skip — the upload
/// proceeds and any true conflict surfaces from the PUT itself.
enum ArtifactPresence {
    /// The path holds an artifact whose SHA-256 equals the local file's —
    /// re-uploading is a no-op, so the upload is skipped (idempotent re-run).
    PresentMatching,
    /// The path holds an artifact whose SHA-256 differs from the local file's
    /// (immutable-version drift): a re-release would overwrite published bytes.
    PresentDiffering { remote_checksum: String },
    /// The path holds no artifact (404) — upload normally.
    Absent,
    /// Existence/content could not be determined (probe error, missing
    /// checksum header). Upload normally so a real conflict isn't masked.
    Unknown,
}

/// Probe whether `url` already holds this artifact by issuing a HEAD and
/// reading Artifactory's `X-Checksum-Sha256` response header.
///
/// Artifactory returns the stored artifact's SHA-256 in that header on a HEAD
/// of an existing path. A 404 means the path is empty (`Absent`); a 2xx with a
/// matching checksum is `PresentMatching`; a 2xx with a differing checksum is
/// `PresentDiffering`. Any transport error, non-404 error status, or absent
/// checksum header degrades to `Unknown` so the caller uploads rather than
/// risking a false skip. The probe is best-effort and is NOT retried — a flaky
/// HEAD must not block a release; the upstream PUT carries the retry budget and
/// remains the source of truth for genuine conflicts.
fn probe_artifact_presence(
    client: &reqwest::blocking::Client,
    url: &str,
    auth: &UploadAuth<'_>,
    local_checksum: &str,
) -> ArtifactPresence {
    let UploadAuth { username, password } = *auth;
    let mut req = client.head(url);
    if !username.is_empty() && !password.is_empty() {
        req = req.basic_auth(username, Some(password));
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(_) => return ArtifactPresence::Unknown,
    };
    let status = resp.status();
    if status.as_u16() == 404 {
        return ArtifactPresence::Absent;
    }
    if !status.is_success() {
        // 401/403/5xx: can't determine presence; let the PUT decide.
        return ArtifactPresence::Unknown;
    }
    match resp
        .headers()
        .get("X-Checksum-Sha256")
        .and_then(|v| v.to_str().ok())
    {
        Some(remote) if remote.eq_ignore_ascii_case(local_checksum) => {
            ArtifactPresence::PresentMatching
        }
        Some(remote) => ArtifactPresence::PresentDiffering {
            remote_checksum: remote.to_string(),
        },
        // Path exists but no checksum header to compare against.
        None => ArtifactPresence::Unknown,
    }
}

/// Outcome of [`upload_single_artifact`]: whether bytes were PUT or the
/// upload was an idempotent no-op.
#[derive(Debug)]
pub(crate) enum UploadOutcome {
    Uploaded,
    AlreadyPresent,
}

/// Upload a single artifact to the target URL.
///
/// When `overwrite` is false (the default), the path is first probed: an
/// identical artifact already present yields an idempotent skip, and a
/// *differing* artifact at the same path hard-errors (immutable-version
/// drift). When `overwrite` is true, the artifact is PUT unconditionally.
///
/// Drives the per-attempt request through [`retry_http_blocking`], which
/// applies the shared `retry_sync` machinery: transport errors, 5xx
/// responses, and 429s retry per the user's `retry:` config (mirrors
/// per-artifact upload); 4xx responses
/// fast-fail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_single_artifact(
    client: &reqwest::blocking::Client,
    headers: &UploadHeaders<'_>,
    auth: &UploadAuth<'_>,
    artifact: &Artifact,
    overwrite: bool,
    ctx: &Context,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<UploadOutcome> {
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

    // Idempotency gate: skip when an identical artifact is already at the
    // path; bail on content drift. `overwrite: true` opts out and always PUTs.
    if !overwrite {
        match probe_artifact_presence(client, url, auth, &checksum) {
            ArtifactPresence::PresentMatching => {
                log.status(&format!(
                    "skipping {} — already uploaded at {} (sha256 match)",
                    artifact.name(),
                    url
                ));
                return Ok(UploadOutcome::AlreadyPresent);
            }
            ArtifactPresence::PresentDiffering { remote_checksum } => {
                bail!(
                    "artifactory: '{}' already exists at {} with a different sha256 \
                     (remote {}, local {}). Artifact paths are immutable per release; \
                     bump the version or set `overwrite: true` to replace it.",
                    artifact.name(),
                    url,
                    remote_checksum,
                    checksum
                );
            }
            // Absent / Unknown both fall through to the upload below.
            ArtifactPresence::Absent | ArtifactPresence::Unknown => {}
        }
    }

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
        vars.set("ArtifactExt", &artifact.ext());
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
                    "retrying artifactory upload of {art_name} (attempt {attempt})"
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
    Ok(UploadOutcome::Uploaded)
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

/// Tally of what an Artifactory publish run did, so the caller can decide
/// whether the whole run was an idempotent no-op (everything skipped) versus a
/// real publish (at least one upload).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactoryUploadSummary {
    /// Artifacts PUT this run (freshly uploaded or overwritten).
    pub uploaded: usize,
    /// Artifacts skipped because an identical copy already existed.
    pub already_present: usize,
}

impl ArtifactoryUploadSummary {
    /// True when at least one artifact was considered AND every one was an
    /// idempotent skip — the signal the publisher uses to record
    /// `Skipped(AlreadyPublished)` instead of `Succeeded`.
    pub fn is_fully_idempotent_skip(&self) -> bool {
        self.uploaded == 0 && self.already_present > 0
    }
}

/// Upload artifacts to Artifactory via HTTP PUT.
///
/// This is a top-level publisher: it reads from `ctx.config.artifactories`
/// rather than from per-crate publish configs.  Each entry specifies a target
/// URL template, credentials, and optional filters.
pub fn publish_to_artifactory(
    ctx: &Context,
    log: &StageLogger,
) -> Result<ArtifactoryUploadSummary> {
    let mut summary = ArtifactoryUploadSummary::default();
    let entries = match ctx.config.artifactories {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(summary),
    };

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every entry's per-artifact upload (the
    // `retryx` policy is captured once per pipe invocation).
    let policy = ctx.retry_policy();

    for entry in entries {
        let label = format!(
            "artifactory entry '{}'",
            entry.name.as_deref().unwrap_or("<unnamed>")
        );
        if crate::util::should_skip_publisher_with_if(
            ctx,
            entry.skip.as_ref(),
            None,
            entry.if_condition.as_deref(),
            &label,
            log,
        )? {
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

        // Determine checksum header name (default: X-Checksum-SHA256).
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
                name, log.redact(&target_url), mode, method, username
            ));
            if !custom_headers.is_empty() {
                for (k, v) in custom_headers {
                    let rendered_v =
                        crate::util::render_or_warn(ctx, log, "artifactory.headers", v)?;
                    log.status(&format!(
                        "(dry-run) would send custom header {}={}",
                        k,
                        log.redact(&rendered_v)
                    ));
                }
            }
            if entry.client_x509_cert.is_some() {
                log.status("(dry-run) would present a client certificate");
            }
            if entry.client_x509_key.is_some() {
                log.status("(dry-run) would present a client key");
            }
            if entry.trusted_certificates.is_some() {
                log.status("(dry-run) would trust custom certificates");
            }
            log.status(&format!(
                "(dry-run) would send checksum header {}",
                checksum_header
            ));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) would filter to build IDs {:?}", ids));
            }
            if let Some(ref exts) = entry.exts {
                log.status(&format!("(dry-run) would filter to extensions {:?}", exts));
            }
            if include_checksum {
                log.status("(dry-run) would include checksum files");
            }
            if include_signature {
                log.status("(dry-run) would include signature files");
            }
            if include_meta {
                log.status("(dry-run) would include metadata files");
            }
            if custom_artifact_name {
                log.status("(dry-run) would apply custom artifact naming");
            }
            if let Some(ref files) = entry.extra_files {
                log.status(&format!(
                    "(dry-run) would upload {} extra file(s)",
                    files.len()
                ));
            }
            log.status(&format!(
                "(dry-run) would read credentials from {}",
                named_env_var
            ));

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
                "no matching artifactory artifacts for '{}' (mode={})",
                name, mode
            ));
            continue;
        }

        log.status(&format!(
            "uploading {} artifacts to artifactory '{}' (mode={})",
            artifacts.len(),
            name,
            mode
        ));

        let overwrite = entry.overwrite.unwrap_or(false);

        // Upload each artifact
        for artifact in &artifacts {
            let url = render_artifact_url(ctx, target_template, artifact, custom_artifact_name)?;
            match upload_single_artifact(
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
                overwrite,
                ctx,
                &policy,
                log,
            )? {
                UploadOutcome::Uploaded => summary.uploaded += 1,
                UploadOutcome::AlreadyPresent => summary.already_present += 1,
            }
        }

        log.status(&format!("artifactory upload complete for '{}'", name));
    }

    Ok(summary)
}

// ---------------------------------------------------------------------------
// collect_artifactory_targets — evidence helper
// ---------------------------------------------------------------------------

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. The rollback path resolves credentials
/// from env at call time via the existing `ARTIFACTORY_<NAME>_*`
/// ladder; nothing about that flow persists in evidence.
pub(crate) type ArtifactoryTarget = anodizer_core::publish_evidence::ArtifactoryTargetSnapshot;

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

/// Encode the per-target `(entry, url)` pairs into the typed
/// [`PublishEvidenceExtra::Artifactory`] variant. Mirrors the wire
/// shape `{ "artifactory_targets": [...] }` that shipped pre-typed.
pub(crate) fn encode_artifactory_targets(
    targets: &[ArtifactoryTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Artifactory(
        anodizer_core::publish_evidence::ArtifactoryExtra {
            artifactory_targets: targets.to_vec(),
        },
    )
}

/// Decode the typed Artifactory variant into structured targets.
/// Returns an empty vec when the variant doesn't match — rollback
/// then falls back to URL-only deletion against the legacy
/// `ARTIFACTORY_TOKEN` ladder.
pub(crate) fn decode_artifactory_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<ArtifactoryTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Artifactory(a) => a.artifactory_targets.clone(),
        _ => Vec::new(),
    }
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
// git-revert publishers via [`crate::util::ROLLBACK_PARALLELISM`].
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
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Mirrors `resolve_http_credentials` (anonymous_ok = false): per
        // entry, each of username/password comes from the templated config
        // value or the `ARTIFACTORY_<NAME>_{USERNAME,SECRET}` env pair.
        let mut out = Vec::new();
        for entry in ctx.config.artifactories.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            let name_upper = entry
                .name
                .as_deref()
                .unwrap_or("")
                .to_uppercase()
                .replace('-', "_");
            if let Some(req) = crate::publisher_helpers::secret_requirement(
                entry.username.as_deref(),
                &format!("ARTIFACTORY_{}_USERNAME", name_upper),
            ) {
                out.push(req);
            }
            if let Some(req) = crate::publisher_helpers::secret_requirement(
                entry.password.as_deref(),
                &format!("ARTIFACTORY_{}_SECRET", name_upper),
            ) {
                out.push(req);
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let summary = publish_to_artifactory(ctx, &log)?;
        // Every matched artifact was already present at its target path (an
        // idempotent re-run): record a SKIP, not a fresh publish.
        if summary.is_fully_idempotent_skip() {
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
                anodizer_core::SkipReason::AlreadyPublished,
            ));
        }
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
        let token_env = ctx
            .env_var("ARTIFACTORY_TOKEN")
            .or_else(|| ctx.env_var("ARTIFACTORY_SECRET"));
        let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "artifactory rollback failed to build HTTP client: {}; manual cleanup required",
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
            "artifactory rollback deleted {} artifact(s), {} already absent, {} failure(s)",
            deleted, already_absent, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn skips_on_nightly(&self) -> bool {
        // Artifact repositories support versioned paths; nightly re-uploads
        // do not clobber stable content and are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
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
                    log.status(&format!("DELETE {}", url));
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
                                        "DELETE {} returned HTTP {} (already absent)",
                                        url, status
                                    ));
                                }
                                DeleteOutcome::Failed(_) => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.2 += 1;
                                    log.warn(&format!(
                                        "DELETE {} returned HTTP {} (manual cleanup may be required)",
                                        url, status
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                            c.2 += 1;
                            log.warn(&format!(
                                "DELETE {} transport error: {} (manual cleanup may be required)",
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
            log.warn("artifactory mutex poisoned by worker panic; reporting counters as-of poison");
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

    /// Defense-in-depth: a custom header whose value is a rendered env-var
    /// secret (e.g. `X-Api-Key: {{ .Env.JFROG_TOKEN }}`) must NOT leak the
    /// actual token value into dry-run log output. The fix wraps the rendered
    /// value in `log.redact()` before the status call.
    #[test]
    fn test_artifactory_dry_run_custom_header_token_is_redacted() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut headers = HashMap::new();
        // Literal header value (not a template) — simulates the rendered output
        // of `{{ .Env.JFROG_TOKEN }}` after template expansion.
        headers.insert(
            "X-Api-Key".to_string(),
            "ghp_ARTIFACTORY_FAKE_SECRET_TOKEN".to_string(),
        );
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            custom_headers: Some(headers),
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        // Inject the secret into the template-vars env so the logger's
        // redaction engine knows to replace its value.
        ctx.template_vars_mut()
            .set_env("JFROG_TOKEN", "ghp_ARTIFACTORY_FAKE_SECRET_TOKEN");
        let log = ctx
            .logger("artifactory")
            .with_capture_handle(capture.clone());
        assert!(publish_to_artifactory(&ctx, &log).is_ok());

        let all_msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
        for msg in &all_msgs {
            assert!(
                !msg.contains("ghp_ARTIFACTORY_FAKE_SECRET_TOKEN"),
                "secret token must not appear in dry-run log output: {msg}"
            );
        }
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
    fn render_artifact_url_interpolates_os_arch_target_ext() {
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
        // ArtifactExt already carries its leading dot (".tar.gz").
        let url = render_artifact_url(
            &ctx,
            "https://art.example.com/{{ .Os }}/{{ .Arch }}/{{ .Target }}{{ .ArtifactExt }}",
            &artifact,
            true,
        )
        .unwrap();
        assert_eq!(
            url,
            "https://art.example.com/linux/amd64/x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn render_artifact_url_template_referencing_artifact_name_suppresses_append() {
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
        // custom_artifact_name=false BUT the template names ArtifactName ->
        // no second append (the name appears exactly once).
        let url = render_artifact_url(
            &ctx,
            "https://art.example.com/repo/{{ .ArtifactName }}",
            &artifact,
            false,
        )
        .unwrap();
        assert_eq!(url, "https://art.example.com/repo/myapp-1.0.0.tar.gz");
    }

    #[test]
    fn render_artifact_url_keeps_single_slash_when_template_trailing_slashed() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        let artifact = Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp.tar.gz".to_string(),
            path: PathBuf::from("dist/myapp.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let url =
            render_artifact_url(&ctx, "https://art.example.com/repo/", &artifact, false).unwrap();
        assert_eq!(url, "https://art.example.com/repo/myapp.tar.gz");
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

    // -----------------------------------------------------------------
    // Live HTTP path tests (scripted responder)
    //
    // The in-process responder records (method, path, body) for every
    // request. Header capture is not available, so credential/checksum
    // header assertions go through `resolve_http_credentials` directly
    // (covered below) while the wire-shape assertions here pin method,
    // path, uploaded body bytes, and retry count.
    // -----------------------------------------------------------------

    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use std::net::SocketAddr;
    use std::time::Duration;

    fn fast_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    /// Build a throwaway file artifact on disk so `upload_single_artifact`
    /// can hash + read it. Returns the tempdir guard (keep alive) and the
    /// constructed `Artifact`.
    fn file_artifact(contents: &[u8], name: &str) -> (tempfile::TempDir, Artifact) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, contents).unwrap();
        let art = Artifact {
            kind: ArtifactKind::Archive,
            name: name.to_string(),
            path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        (dir, art)
    }

    fn upload_ctx() -> Context {
        Context::new(Config::default(), ContextOptions::default())
    }

    fn no_headers() -> HashMap<String, String> {
        HashMap::new()
    }

    /// PUT upload to a 201-route: the responder records exactly one
    /// request, with method PUT, the rendered path, and the file bytes as
    /// the body.
    #[test]
    fn upload_put_sends_file_body_to_target() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/myapp.tar.gz",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"payload-bytes", "myapp.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/myapp.tar.gz");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: &url,
                checksum_header: "X-Checksum-SHA256",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(3),
            &log,
        )
        .expect("201 upload succeeds");

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one request: {entries:?}");
        assert_eq!(entries[0].method, "PUT");
        assert_eq!(entries[0].path, "/repo/myapp.tar.gz");
        assert_eq!(
            entries[0].body, "payload-bytes",
            "the file bytes are the request body"
        );
    }

    /// POST method routes the request as a POST (not PUT). Pins that the
    /// configured method actually selects `client.post`.
    #[test]
    fn upload_post_uses_post_verb() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repo/app.bin",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"x", "app.bin");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/app.bin");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "POST",
                url: &url,
                checksum_header: "",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(1),
            &log,
        )
        .expect("200 POST succeeds");
        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, "POST");
    }

    /// A 503 on the first attempt retries and the second attempt (200)
    /// succeeds — exactly two requests reach the wire.
    #[test]
    fn upload_retries_5xx_then_succeeds() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "PUT",
                path_pattern: "/repo/r.tar.gz",
                response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
                times: Some(1),
            },
            ScriptedRoute {
                method: "PUT",
                path_pattern: "/repo/r.tar.gz",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);
        let (_dir, art) = file_artifact(b"retry-body", "r.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/r.tar.gz");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: &url,
                checksum_header: "X-Checksum-SHA256",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(3),
            &log,
        )
        .expect("retry recovers from 503");
        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 2, "one 503 + one 201 = two attempts");
    }

    /// 5xx on every attempt exhausts the retry budget and surfaces an
    /// error naming the artifact, method, and status. The number of
    /// requests equals `max_attempts`.
    #[test]
    fn upload_5xx_exhausts_retries_and_errors() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/e.tar.gz",
            response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"e", "e.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/e.tar.gz");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: &url,
                checksum_header: "X-Checksum-SHA256",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(3),
            &log,
        )
        .expect_err("persistent 500 must exhaust and error");
        let chain = format!("{err:#}");
        assert!(chain.contains("e.tar.gz"), "names artifact: {chain}");
        assert!(chain.contains("500"), "carries upstream status: {chain}");

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 3, "all three attempts hit the wire");
    }

    /// A 4xx (e.g. 403) fast-fails: no retry, exactly one request, and the
    /// decoded Artifactory error envelope reaches the error message.
    #[test]
    fn upload_4xx_fast_fails_without_retry() {
        let body = r#"{"errors":[{"status":403,"message":"forbidden path"}]}"#;
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        );
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/f.tar.gz",
            response: resp,
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"f", "f.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/f.tar.gz");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: &url,
                checksum_header: "X-Checksum-SHA256",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(5),
            &log,
        )
        .expect_err("403 must fast-fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("forbidden path"),
            "decoded envelope message present: {chain}"
        );
        assert!(chain.contains("403"), "status present: {chain}");

        let entries = log_recorder.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "4xx must NOT retry despite max_attempts=5: {entries:?}"
        );
    }

    /// An unsupported HTTP method fails fast OUTSIDE the retry loop — no
    /// request is ever sent.
    #[test]
    fn upload_rejects_unsupported_method() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/x.tar.gz",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"x", "x.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/x.tar.gz");
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "DELETE",
                url: &url,
                checksum_header: "",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(3),
            &log,
        )
        .expect_err("DELETE is not a supported upload method");
        assert!(
            err.to_string().contains("unsupported HTTP method"),
            "unexpected: {err}"
        );
        assert!(
            log_recorder.lock().unwrap().is_empty(),
            "no request must reach the wire for a bad method"
        );
    }

    /// A missing artifact file bails before any network activity.
    #[test]
    fn upload_missing_file_bails() {
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let art = Artifact {
            kind: ArtifactKind::Archive,
            name: "gone.tar.gz".to_string(),
            path: PathBuf::from("/nonexistent/anodizer/gone.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: "http://127.0.0.1:1/repo/gone.tar.gz",
                checksum_header: "",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(1),
            &log,
        )
        .expect_err("missing file must bail");
        assert!(
            err.to_string().contains("artifact file not found"),
            "unexpected: {err}"
        );
    }

    /// A directory passed as an artifact path is rejected (can't upload a
    /// directory) before any network call.
    #[test]
    fn upload_directory_path_bails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let art = Artifact {
            kind: ArtifactKind::Archive,
            name: "adir".to_string(),
            path: dir.path().to_path_buf(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let custom = no_headers();
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: "http://127.0.0.1:1/repo/adir",
                checksum_header: "",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(1),
            &log,
        )
        .expect_err("directory upload must bail");
        assert!(
            err.to_string().contains("can't be a directory"),
            "unexpected: {err}"
        );
    }

    /// A custom header carrying broken template syntax fails fast (outside
    /// the retry loop) rather than pushing an unrendered `{{ }}` literal
    /// onto the wire.
    #[test]
    fn upload_bad_custom_header_template_fails_fast() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/h.tar.gz",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (_dir, art) = file_artifact(b"h", "h.tar.gz");
        let ctx = upload_ctx();
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let url = format!("http://{addr}/repo/h.tar.gz");
        let mut custom = HashMap::new();
        // Unknown filter is a hard render error (not undefined-var leniency).
        custom.insert(
            "X-Bad".to_string(),
            "{{ ArtifactName | nonexistent_filter }}".to_string(),
        );
        let client = build_reqwest_client(None, None, None).unwrap();
        let err = upload_single_artifact(
            &client,
            &UploadHeaders {
                method: "PUT",
                url: &url,
                checksum_header: "",
                custom_headers: &custom,
            },
            &UploadAuth {
                username: "",
                password: "",
            },
            &art,
            true,
            &ctx,
            &fast_policy(3),
            &log,
        )
        .expect_err("bad header template must fail-fast");
        assert!(
            err.to_string().contains("custom header 'X-Bad'"),
            "unexpected: {err}"
        );
        assert!(
            log_recorder.lock().unwrap().is_empty(),
            "render failure must abort before any request"
        );
    }

    /// End-to-end through `publish_to_artifactory` in LIVE mode: the
    /// per-entry name + ArtifactName rendering produces the correct PUT
    /// path against the responder, exercising client build + render +
    /// upload in one flow. Credentials come from config (so no env race).
    #[test]
    fn publish_live_uploads_artifact_to_responder() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![
            // Idempotency probe: path is empty (404) → upload proceeds.
            ScriptedRoute {
                method: "HEAD",
                path_pattern: "/repo/live-app.tar.gz",
                response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "PUT",
                path_pattern: "/repo/live-app.tar.gz",
                response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);
        let dir = tempfile::tempdir().unwrap();
        let art_path = dir.path().join("live-app.tar.gz");
        fs::write(&art_path, b"live-bytes").unwrap();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.retry = Some(anodizer_core::config::RetryConfig {
            attempts: 2,
            delay: anodizer_core::config::HumanDuration(Duration::from_millis(1)),
            max_delay: anodizer_core::config::HumanDuration(Duration::from_millis(2)),
        });
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some(format!("http://{addr}/repo/")),
            username: Some("deployer".to_string()),
            password: Some("hunter2".to_string()),
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "live-app.tar.gz".to_string(),
            path: art_path,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        let log = ctx.logger("artifactory");
        publish_to_artifactory(&ctx, &log).expect("live publish succeeds");

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 2, "HEAD probe + PUT upload: {entries:?}");
        assert_eq!(entries[0].method, "HEAD", "presence probe first");
        assert_eq!(entries[0].path, "/repo/live-app.tar.gz");
        assert_eq!(entries[1].method, "PUT");
        assert_eq!(entries[1].path, "/repo/live-app.tar.gz");
        assert_eq!(entries[1].body, "live-bytes");
    }

    /// Build a single-artifact live publish context against `addr`, with the
    /// given `overwrite` setting. Returns the context and the on-disk bytes'
    /// hex SHA-256 so the test can script a matching / differing HEAD probe.
    fn live_publish_ctx(addr: SocketAddr, overwrite: Option<bool>) -> (Context, String) {
        let dir = tempfile::tempdir().unwrap();
        let art_path = dir.path().join("idem-app.tar.gz");
        fs::write(&art_path, b"idem-bytes").unwrap();
        let checksum = sha256_file(&art_path).unwrap();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.retry = Some(anodizer_core::config::RetryConfig {
            attempts: 2,
            delay: anodizer_core::config::HumanDuration(Duration::from_millis(1)),
            max_delay: anodizer_core::config::HumanDuration(Duration::from_millis(2)),
        });
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some(format!("http://{addr}/repo/")),
            username: Some("deployer".to_string()),
            password: Some("hunter2".to_string()),
            overwrite,
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "idem-app.tar.gz".to_string(),
            path: art_path,
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        // Keep the tempdir alive for the duration of the test by leaking it;
        // the file must outlive the upload read. Tests are short-lived.
        std::mem::forget(dir);
        (ctx, checksum)
    }

    /// Idempotent re-run: the path already holds an artifact whose sha256
    /// matches the local file → HEAD probe returns the match, the PUT is
    /// skipped entirely, and the run is a no-op upload.
    #[test]
    fn publish_skips_when_identical_artifact_already_present() {
        // Bind first so the responder can echo back the right checksum header.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (ctx, checksum) = live_publish_ctx(addr, None);
        let head_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nX-Checksum-Sha256: {checksum}\r\nContent-Length: 0\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (_addr, log_recorder) =
            anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder_on(
                listener,
                move |_| {
                    vec![ScriptedRoute {
                        method: "HEAD",
                        path_pattern: "/repo/idem-app.tar.gz",
                        response: head_resp,
                        times: None,
                    }]
                },
            );

        let log = ctx.logger("artifactory");
        let summary = publish_to_artifactory(&ctx, &log).expect("idempotent re-run is ok");
        assert_eq!(summary.uploaded, 0, "nothing uploaded");
        assert_eq!(summary.already_present, 1, "one artifact skipped");
        assert!(summary.is_fully_idempotent_skip());

        let entries = log_recorder.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "only the HEAD probe — no PUT: {entries:?}"
        );
        assert_eq!(entries[0].method, "HEAD");
    }

    /// A path already holding a *different* artifact for the same version is
    /// immutable-version drift: the publish must hard-error, NOT silently
    /// overwrite, when `overwrite` is unset.
    #[test]
    fn publish_bails_on_content_drift_without_overwrite() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "HEAD",
            path_pattern: "/repo/idem-app.tar.gz",
            response: "HTTP/1.1 200 OK\r\nX-Checksum-Sha256: \
                       0000000000000000000000000000000000000000000000000000000000000000\
                       \r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (ctx, _checksum) = live_publish_ctx(addr, None);
        let log = ctx.logger("artifactory");
        let err = publish_to_artifactory(&ctx, &log).expect_err("content drift must error");
        let chain = format!("{err:#}");
        assert!(chain.contains("different sha256"), "{chain}");
        assert!(chain.contains("overwrite: true"), "{chain}");
    }

    /// `overwrite: true` skips the existence probe and PUTs unconditionally —
    /// restoring blind-overwrite for repos that allow it.
    #[test]
    fn publish_overwrite_true_skips_probe_and_puts() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/idem-app.tar.gz",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let (ctx, _checksum) = live_publish_ctx(addr, Some(true));
        let log = ctx.logger("artifactory");
        let summary = publish_to_artifactory(&ctx, &log).expect("overwrite publish ok");
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.already_present, 0);

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 1, "no HEAD probe, just the PUT: {entries:?}");
        assert_eq!(entries[0].method, "PUT");
    }

    /// Live mode with no matching artifacts short-circuits without firing
    /// any HTTP request (the "no matching artifacts" branch).
    #[test]
    fn publish_live_no_artifacts_makes_no_request() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/whatever",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some(format!("http://{addr}/repo/")),
            username: Some("u".to_string()),
            password: Some("p".to_string()),
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
        publish_to_artifactory(&ctx, &log).expect("no artifacts is ok");
        assert!(
            log_recorder.lock().unwrap().is_empty(),
            "no artifacts => no upload request"
        );
    }

    // -----------------------------------------------------------------
    // build_reqwest_client — mTLS + trusted-CA error/success paths
    // -----------------------------------------------------------------

    /// A non-existent client cert path surfaces a read error naming the
    /// path (the `failed to read client cert` branch).
    #[test]
    fn build_client_missing_cert_file_errors() {
        let err = build_reqwest_client(
            Some("/nonexistent/anodizer/cert.pem"),
            Some("/nonexistent/anodizer/key.pem"),
            None,
        )
        .expect_err("missing cert file must error");
        assert!(
            err.to_string().contains("failed to read client cert"),
            "unexpected: {err}"
        );
    }

    /// A cert file that exists but holds garbage (not a PEM identity)
    /// fails at `Identity::from_pem` with the identity-load message.
    #[test]
    fn build_client_bad_pem_identity_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        fs::write(&cert, b"not a real pem").unwrap();
        fs::write(&key, b"also not a pem").unwrap();
        let err = build_reqwest_client(
            Some(cert.to_str().unwrap()),
            Some(key.to_str().unwrap()),
            None,
        )
        .expect_err("garbage PEM must fail identity load");
        assert!(
            err.to_string()
                .contains("failed to load client certificate identity"),
            "unexpected: {err}"
        );
    }

    /// Only one of cert/key set is rejected as an incoherent mTLS pair.
    #[test]
    fn build_client_half_mtls_pair_errors() {
        let err = build_reqwest_client(Some("/tmp/cert.pem"), None, None)
            .expect_err("half mTLS pair must error");
        assert!(
            err.to_string().contains("must both be set"),
            "unexpected: {err}"
        );
    }

    /// A set-but-empty (whitespace) trusted-certificates bundle is
    /// rejected with the copy-paste-accident guidance rather than
    /// installing an empty trust store.
    #[test]
    fn build_client_empty_trusted_certs_errors() {
        let err = build_reqwest_client(None, None, Some("   \n\t "))
            .expect_err("blank CA bundle must error");
        assert!(
            err.to_string()
                .contains("trusted_certificates is set but empty"),
            "unexpected: {err}"
        );
    }

    /// A non-blank trusted-certificates value that contains no parseable
    /// PEM certificate is rejected with the truncation guidance.
    #[test]
    fn build_client_unparseable_trusted_certs_errors() {
        let err = build_reqwest_client(None, None, Some("garbage-not-a-cert"))
            .expect_err("unparseable CA bundle must error");
        let msg = err.to_string();
        assert!(
            msg.contains("trusted_certificates"),
            "error must name the field: {msg}"
        );
    }

    /// No mTLS and no CA bundle builds a plain client successfully — the
    /// happy path through `build_reqwest_client`.
    #[test]
    fn build_client_plain_succeeds() {
        assert!(build_reqwest_client(None, None, None).is_ok());
    }

    // -----------------------------------------------------------------
    // Credential cascade via resolve_http_credentials (env override)
    // -----------------------------------------------------------------

    /// With no config credentials, the per-entry env vars
    /// `ARTIFACTORY_PROD_USERNAME` / `_SECRET` resolve the basic-auth
    /// pair. Confirms the prefix + uppercased-name env ladder.
    #[test]
    #[serial_test::serial]
    fn credentials_resolve_from_named_env_vars() {
        use anodizer_core::test_helpers::env::env_mutex;
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex; paired set/remove below.
        unsafe {
            std::env::set_var("ARTIFACTORY_PROD_USERNAME", "envuser");
            std::env::set_var("ARTIFACTORY_PROD_SECRET", "envsecret");
        }
        let ctx = upload_ctx();
        let (u, p) = crate::http_upload::resolve_http_credentials(
            &ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "artifactory",
                entry_name: "prod",
                config_username: None,
                config_password: None,
                env_prefix: "ARTIFACTORY",
                anonymous_ok: false,
            },
        )
        .expect("env creds resolve");
        unsafe {
            std::env::remove_var("ARTIFACTORY_PROD_USERNAME");
            std::env::remove_var("ARTIFACTORY_PROD_SECRET");
        }
        assert_eq!(u, "envuser");
        assert_eq!(p, "envsecret");
    }

    /// A hyphenated entry name is folded to `_` and upper-cased for the
    /// env lookup, so `my-repo` reads `ARTIFACTORY_MY_REPO_SECRET`.
    #[test]
    #[serial_test::serial]
    fn credentials_fold_hyphen_in_entry_name() {
        use anodizer_core::test_helpers::env::env_mutex;
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised by env_mutex; paired set/remove below.
        unsafe {
            std::env::set_var("ARTIFACTORY_MY_REPO_USERNAME", "hu");
            std::env::set_var("ARTIFACTORY_MY_REPO_SECRET", "hp");
        }
        let ctx = upload_ctx();
        let (u, p) = crate::http_upload::resolve_http_credentials(
            &ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "artifactory",
                entry_name: "my-repo",
                config_username: None,
                config_password: None,
                env_prefix: "ARTIFACTORY",
                anonymous_ok: false,
            },
        )
        .expect("hyphen-folded env creds resolve");
        unsafe {
            std::env::remove_var("ARTIFACTORY_MY_REPO_USERNAME");
            std::env::remove_var("ARTIFACTORY_MY_REPO_SECRET");
        }
        assert_eq!(u, "hu");
        assert_eq!(p, "hp");
    }

    /// Anonymous resolution (no config, no env) is refused when
    /// `anonymous_ok = false` — the live artifactory path's guard.
    #[test]
    #[serial_test::serial]
    fn credentials_refuse_anonymous_when_required() {
        use anodizer_core::test_helpers::env::env_mutex;
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialised; ensure no stale env leaks into the lookup.
        unsafe {
            std::env::remove_var("ARTIFACTORY_LONELY_USERNAME");
            std::env::remove_var("ARTIFACTORY_LONELY_SECRET");
        }
        let ctx = upload_ctx();
        let err = crate::http_upload::resolve_http_credentials(
            &ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "artifactory",
                entry_name: "lonely",
                config_username: None,
                config_password: None,
                env_prefix: "ARTIFACTORY",
                anonymous_ok: false,
            },
        )
        .expect_err("anonymous must be refused");
        assert!(
            err.to_string().contains("anonymous upload is refused"),
            "unexpected: {err}"
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
        // Empty evidence drives rollback into the no-targets branch.
        // The capture pins that production actually invoked `log.warn`
        // with the helper-formatted message — a hand-constructed expected
        // string compared against the helper output would pass even if
        // the rollback body forgot the warn entirely.
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("artifactory");
        let p = ArtifactoryPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("artifactory")
                && m.contains("upload URLs")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// The empty-evidence warn text comes from the shared helper. Tests
    /// across the Assets-group publishers reuse this helper so the
    /// message wording can be pinned in one place.
    #[test]
    fn artifactory_rollback_empty_warning_msg_shape() {
        let msg =
            crate::publisher_helpers::rollback_empty_warning_msg("artifactory", "upload URLs");
        assert!(
            msg.starts_with("no upload URLs recorded in artifactory evidence"),
            "{msg}"
        );
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

    #[test]
    fn artifactory_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence with a populated
        // variant and assert (a) no credential-shaped keys appear AND
        // (b) the operator-public upload coordinates are preserved.
        let mut e = anodizer_core::PublishEvidence::new("artifactory");
        e.extra = encode_artifactory_targets(&[ArtifactoryTarget {
            entry: "prod".into(),
            url: "https://art.example.com/repo/foo.tar.gz".into(),
        }]);
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"username\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: operator-public coordinates present.
        assert!(s.contains("\"entry\":\"prod\""), "{s}");
        assert!(
            s.contains("\"url\":\"https://art.example.com/repo/foo.tar.gz\""),
            "{s}"
        );
    }

    /// A non-Artifactory variant decodes to an empty vec so rollback
    /// falls back to URL-only deletion without panicking.
    #[test]
    fn artifactory_rollback_target_extra_tolerates_missing_field() {
        assert!(decode_artifactory_targets(&anodizer_core::PublishEvidenceExtra::Empty).is_empty());
        // Wrong variant: a homebrew evidence is not an artifactory
        // evidence — defensive isolation between publishers.
        let homebrew = anodizer_core::PublishEvidenceExtra::Homebrew(
            anodizer_core::publish_evidence::HomebrewExtra {
                homebrew_targets: Vec::new(),
            },
        );
        assert!(decode_artifactory_targets(&homebrew).is_empty());
    }

    // -----------------------------------------------------------------
    // parallel_delete — live DELETE fan-out classification
    // -----------------------------------------------------------------

    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    fn delete_client() -> reqwest::blocking::Client {
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(5)).expect("client")
    }

    /// A 2xx DELETE is counted as deleted and the request reaches the
    /// wire as an actual HTTP DELETE.
    #[test]
    fn parallel_delete_2xx_counts_as_deleted() {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repo/gone.tar.gz",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let jobs = vec![RollbackJob {
            url: format!("http://{addr}/repo/gone.tar.gz"),
            basic_auth: Some(("u".to_string(), "p".to_string())),
            bearer: None,
        }];
        let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
        assert_eq!((deleted, absent, failed), (1, 0, 0));

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, "DELETE");
        assert_eq!(entries[0].path, "/repo/gone.tar.gz");
    }

    /// A 404 DELETE classifies as already-absent (not failed), so a
    /// re-run after a partial rollback doesn't print phantom failures.
    #[test]
    fn parallel_delete_404_counts_as_already_absent() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repo/missing.tar.gz",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let jobs = vec![RollbackJob {
            url: format!("http://{addr}/repo/missing.tar.gz"),
            basic_auth: None,
            bearer: Some("tok".to_string()),
        }];
        let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
        assert_eq!((deleted, absent, failed), (0, 1, 0));
    }

    /// A 5xx DELETE classifies as failed and emits an operator-facing
    /// warn naming the URL.
    #[test]
    fn parallel_delete_5xx_counts_as_failed_and_warns() {
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repo/boom.tar.gz",
            response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let capture = anodizer_core::log::LogCapture::new();
        let log =
            StageLogger::new("artifactory", Verbosity::Quiet).with_capture_handle(capture.clone());
        let url = format!("http://{addr}/repo/boom.tar.gz");
        let jobs = vec![RollbackJob {
            url: url.clone(),
            basic_auth: Some(("u".to_string(), "p".to_string())),
            bearer: None,
        }];
        let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
        assert_eq!((deleted, absent, failed), (0, 0, 1));
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("boom.tar.gz") && m.contains("manual cleanup")),
            "expected failed-DELETE warn naming the URL; got: {:?}",
            capture.warn_messages()
        );
    }

    /// A transport error (connection refused — no responder listening)
    /// counts as failed and emits a transport-error warn.
    #[test]
    fn parallel_delete_transport_error_counts_as_failed() {
        let capture = anodizer_core::log::LogCapture::new();
        let log =
            StageLogger::new("artifactory", Verbosity::Quiet).with_capture_handle(capture.clone());
        // Port 1 on loopback refuses connections.
        let jobs = vec![RollbackJob {
            url: "http://127.0.0.1:1/repo/unreachable.tar.gz".to_string(),
            basic_auth: None,
            bearer: Some("tok".to_string()),
        }];
        let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
        assert_eq!((deleted, absent, failed), (0, 0, 1));
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("transport error")),
            "expected transport-error warn; got: {:?}",
            capture.warn_messages()
        );
    }

    /// A mixed batch larger than ROLLBACK_PARALLELISM exercises the
    /// chunked fan-out and aggregates every bucket correctly.
    #[test]
    fn parallel_delete_mixed_batch_aggregates_all_buckets() {
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/ok1",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/ok2",
                response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/gone",
                response: "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
            ScriptedRoute {
                method: "DELETE",
                path_pattern: "/bad",
                response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);
        let log = StageLogger::new("artifactory", Verbosity::Quiet);
        let mk = |p: &str| RollbackJob {
            url: format!("http://{addr}{p}"),
            basic_auth: Some(("u".to_string(), "p".to_string())),
            bearer: None,
        };
        // Five jobs > ROLLBACK_PARALLELISM (4) so the chunking loop runs
        // more than once. `/ok1` repeated lands two deletes.
        let jobs = vec![mk("/ok1"), mk("/ok2"), mk("/gone"), mk("/bad"), mk("/ok1")];
        let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
        assert_eq!(deleted, 3, "ok1 + ok2 + ok1");
        assert_eq!(absent, 1, "410 Gone");
        assert_eq!(failed, 1, "403 Forbidden");
    }

    /// Full rollback through the Publisher trait: structured evidence
    /// resolves per-entry basic auth and issues a live DELETE that the
    /// responder records, then logs the summary line.
    #[test]
    fn rollback_issues_delete_for_recorded_url() {
        use anodizer_core::config::{ArtifactoryConfig, Config};
        use anodizer_core::context::ContextOptions;
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repo/foo.tar.gz",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        }]);
        let url = format!("http://{addr}/repo/foo.tar.gz");

        let mut config = Config::default();
        config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some(format!("http://{addr}/repo/")),
            username: Some("deployer".to_string()),
            password: Some("hunter2".to_string()),
            ..Default::default()
        }]);
        let mut ctx = Context::new(config, ContextOptions::default());

        let mut evidence = PublishEvidence::new("artifactory");
        evidence.artifact_paths = vec![std::path::PathBuf::from(&url)];
        evidence.extra = encode_artifactory_targets(&[ArtifactoryTarget {
            entry: "prod".to_string(),
            url: url.clone(),
        }]);

        let p = ArtifactoryPublisher::new();
        p.rollback(&mut ctx, &evidence).expect("rollback ok");

        let entries = log_recorder.lock().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].method, "DELETE");
        assert_eq!(entries[0].path, "/repo/foo.tar.gz");
    }
}
