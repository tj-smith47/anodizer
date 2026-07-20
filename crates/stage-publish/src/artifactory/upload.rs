use super::*;

// ---------------------------------------------------------------------------
// validate_upload_mode
// ---------------------------------------------------------------------------

/// Validate the upload mode string. Only `"archive"` and `"binary"` are
/// accepted; matching is case-insensitive so `mode: Archive` works.
///
/// `publisher` prefixes the error so the same shared validator serves both the
/// Artifactory and the generic `uploads:` publisher with a correct label.
pub fn validate_upload_mode_for(publisher: &str, mode: &str) -> Result<()> {
    match mode.to_ascii_lowercase().as_str() {
        "archive" | "binary" => Ok(()),
        _ => bail!(
            "{}: invalid upload mode '{}' (expected 'archive' or 'binary')",
            publisher,
            mode
        ),
    }
}

/// Validate the upload mode for the Artifactory publisher (label `artifactory`).
pub fn validate_upload_mode(mode: &str) -> Result<()> {
    validate_upload_mode_for("artifactory", mode)
}

// ---------------------------------------------------------------------------
// Artifact filtering by mode
// ---------------------------------------------------------------------------

/// Return the artifact kinds that match the given upload mode.
/// `binary` selects compiled binaries; everything else selects every
/// uploadable artifact kind.
pub(crate) fn artifact_kinds_for_mode(mode: &str) -> Vec<ArtifactKind> {
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

/// Collect artifacts matching mode, optional ID filter, optional extension
/// filter, and optional `exclude:` glob filter.
/// Also collects checksum/signature/metadata artifacts and extra files when configured.
pub(crate) fn collect_upload_artifacts<'a>(
    ctx: &'a Context,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
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
            // A macOS `.app` bundle is a DIRECTORY; uploading it as a file dies
            // with "the asset to upload can't be a directory". Its wrapping
            // `.dmg`/`.pkg` (both files) are the correct upload subjects.
            if anodizer_core::artifact::is_directory_bundle_artifact(a) {
                return false;
            }
            // ID filter
            if !crate::util::matches_id_filter(a, ids) {
                return false;
            }
            // `exclude:` glob filter — drop sidecars (checksums/sigs/SBOMs)
            // the operator keeps off THIS Artifactory target.
            if !anodizer_core::artifact::passes_exclude_filter(a, exclude) {
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

/// Resolve an upload entry's `extra_files` specs into synthetic
/// [`ArtifactKind::UploadableFile`] artifacts, mirroring GoReleaser's
/// `extrafiles.Find` (glob expansion, optional per-file `name_template`
/// override, directory filtering, path de-duplication).
///
/// `publisher` labels any error so the same shared resolver serves both the
/// Artifactory and the generic `uploads:` publisher with a correct prefix.
/// The `name_template` (when set) is rendered through the context's template
/// vars so a user can write `name_template: "{{ .ProjectName }}-extra.txt"`;
/// when unset, the file's base name is used (GoReleaser's default). The
/// resulting artifacts carry no build target, so the per-artifact URL renders
/// without `Os`/`Arch`/`Target` bindings — matching how a non-build asset
/// uploads.
pub(crate) fn resolve_extra_file_artifacts(
    ctx: &Context,
    publisher: &str,
    specs: &[anodizer_core::config::ExtraFileSpec],
    log: &StageLogger,
) -> Result<Vec<Artifact>> {
    let resolved = anodizer_core::extrafiles::resolve(specs, log)
        .with_context(|| format!("{publisher}: resolve extra_files"))?;
    let mut out = Vec::with_capacity(resolved.len());
    for r in resolved {
        // Render the optional name override; fall back to the file's base name
        // (GoReleaser's default when no name_template is set).
        let name = match r.name_template.as_deref() {
            Some(tmpl) => ctx.render_template(tmpl).with_context(|| {
                format!("{publisher}: render extra_files name_template '{tmpl}'")
            })?,
            None => r
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        out.push(Artifact {
            kind: ArtifactKind::UploadableFile,
            path: r.path,
            name,
            target: None,
            crate_name: ctx.config.project_name.clone(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }
    Ok(out)
}

/// Collect an upload entry's full owned artifact set: the mode/ids/exts-filtered
/// release artifacts (plus checksum/signature/meta sidecars) **and** the
/// entry's `extra_files` specs resolved into uploadable artifacts.
///
/// This is the GoReleaser-parity entry point both HTTP-upload publishers drive
/// (`uploadWithFilter` in GoReleaser appends `extrafiles.Find` results to the
/// filtered set on every run). Resolved `extra_files` artifacts are ALWAYS
/// included — with `extra_files_only: true` they are returned alongside any
/// pre-registered [`ArtifactKind::UploadableFile`] artifacts (e.g. from the
/// `template_files:` stage) and the mode-filtered release set is skipped;
/// otherwise they are appended to it. De-duplicates by on-disk path so a file
/// matched by both a glob and a pre-registered artifact uploads once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_upload_artifacts_owned(
    ctx: &Context,
    publisher: &str,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
    extra_files: Option<&[anodizer_core::config::ExtraFileSpec]>,
    log: &StageLogger,
) -> Result<Vec<Artifact>> {
    let mut out: Vec<Artifact> = collect_upload_artifacts(ctx, mode, ids, exclude, exts, flags)
        .into_iter()
        .cloned()
        .collect();

    if let Some(specs) = extra_files
        && !specs.is_empty()
    {
        let resolved = resolve_extra_file_artifacts(ctx, publisher, specs, log)?;
        let mut seen: std::collections::HashSet<std::path::PathBuf> =
            out.iter().map(|a| a.path.clone()).collect();
        for a in resolved {
            if seen.insert(a.path.clone()) {
                out.push(a);
            }
        }
    }

    Ok(out)
}

/// Collect an upload entry's owned artifact set for **rollback-target
/// enumeration**, degrading to the mode/ids/exts-filtered set when
/// `extra_files` resolution fails.
///
/// Used by both publishers' `collect_*_targets` evidence walkers. A quiet
/// logger swallows the `extra_files` glob warnings (rollback enumeration is
/// not a user-facing render pass), and a resolution error only narrows the
/// rollback checklist — the publish path itself called
/// [`collect_upload_artifacts_owned`] with `?`, so any genuine blocker has
/// already surfaced there. The fallback therefore never hides a publish
/// failure; it just keeps the rollback DELETE list as complete as the
/// resolvable inputs allow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_target_artifacts_best_effort(
    ctx: &Context,
    publisher: &'static str,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
    extra_files: Option<&[anodizer_core::config::ExtraFileSpec]>,
) -> Vec<Artifact> {
    let quiet = StageLogger::new(publisher, anodizer_core::log::Verbosity::Quiet);
    collect_upload_artifacts_owned(
        ctx,
        publisher,
        mode,
        ids,
        exclude,
        exts,
        flags,
        extra_files,
        &quiet,
    )
    .unwrap_or_else(|_| {
        collect_upload_artifacts(ctx, mode, ids, exclude, exts, flags)
            .into_iter()
            .cloned()
            .collect()
    })
}

// ---------------------------------------------------------------------------
// upload_single_artifact
// ---------------------------------------------------------------------------

/// HTTP request descriptor for [`upload_single_artifact_prepared`].
///
/// Bundles the "what URL / how to address it" fields. The `checksum_header`
/// slot, when non-empty, names a custom HTTP header (e.g. `X-Checksum-Sha256`)
/// that is set to the artifact's hex SHA-256 before the request is dispatched.
/// `publisher` labels error messages so the shared upload path attributes a
/// failure to the calling publisher (`artifactory` / `uploads`). User-supplied
/// custom headers are rendered separately (via [`render_custom_headers`]) and
/// passed alongside, since their rendering needs `ctx` and must run in the
/// serial pre-pass ahead of the parallel network fan-out.
#[derive(Clone, Copy)]
pub(crate) struct UploadHeaders<'a> {
    pub(crate) publisher: &'a str,
    pub(crate) method: &'a str,
    pub(crate) url: &'a str,
    pub(crate) checksum_header: &'a str,
}

/// HTTP basic-auth credentials for [`upload_single_artifact`]. Either both
/// fields are non-empty (auth applied) or both are empty (anonymous).
#[derive(Clone, Copy)]
pub(crate) struct UploadAuth<'a> {
    pub(crate) username: &'a str,
    pub(crate) password: &'a str,
}

/// Whether the target path already holds an artifact, and (when present)
/// whether its stored SHA-256 matches the bytes about to be uploaded.
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
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_single_artifact(
    client: &reqwest::blocking::Client,
    headers: &UploadHeaders<'_>,
    auth: &UploadAuth<'_>,
    custom_headers: &HashMap<String, String>,
    artifact: &Artifact,
    overwrite: bool,
    ctx: &Context,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<UploadOutcome> {
    // Header rendering touches `ctx` (not `Sync`); doing it here keeps the
    // post-render upload free of `ctx` so the shared driver can fan the PUTs
    // out across threads after rendering serially.
    let rendered_headers = render_custom_headers(ctx, custom_headers, artifact)?;
    upload_single_artifact_prepared(
        client,
        headers,
        auth,
        artifact,
        overwrite,
        &rendered_headers,
        policy,
        ctx.retry_deadline(),
        log,
    )
}

/// Render each `custom_headers` template value against the artifact-scoped
/// template vars (`ArtifactName`/`ArtifactExt`/`Os`/`Arch`/`Target`).
///
/// A render failure surfaces as a configuration error (bad template syntax,
/// missing variable, …) rather than pushing `{{ ... }}` literals onto the wire
/// — Artifactory typically rejects those with a confusing 400. Pulled out of
/// the upload path so it can run in the serial pre-pass (it needs the non-`Sync`
/// `ctx`) ahead of the parallel network fan-out.
pub(crate) fn render_custom_headers(
    ctx: &Context,
    custom_headers: &HashMap<String, String>,
    artifact: &Artifact,
) -> Result<Vec<(String, String)>> {
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
            format!("rendering custom header '{}' for '{}'", k, artifact.name())
        })?;
        rendered_headers.push((k.clone(), rendered_v));
    }
    Ok(rendered_headers)
}

/// Upload a single artifact whose custom headers were already rendered (so this
/// path is free of the non-`Sync` `ctx` and can run on a worker thread).
///
/// Carries the idempotency probe, content-drift bail, and retry budget. See
/// [`upload_single_artifact`] for the `ctx`-rendering wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_single_artifact_prepared(
    client: &reqwest::blocking::Client,
    headers: &UploadHeaders<'_>,
    auth: &UploadAuth<'_>,
    artifact: &Artifact,
    overwrite: bool,
    rendered_headers: &[(String, String)],
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<UploadOutcome> {
    let UploadHeaders {
        publisher,
        method,
        url,
        checksum_header,
    } = *headers;
    let UploadAuth { username, password } = *auth;
    let path = &artifact.path;
    if !path.exists() {
        bail!("{publisher}: artifact file not found: {}", path.display());
    }
    if path.is_dir() {
        bail!(
            "{publisher}: upload failed: the asset to upload can't be a directory: {}",
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
                // Per-artifact skip detail is verbose-only; the entry summary
                // (default verbosity) reports the aggregate already-present
                // count.
                log.verbose(&format!(
                    "skipped {} (already uploaded, sha256 match) — already present at {}",
                    artifact.name(),
                    url
                ));
                return Ok(UploadOutcome::AlreadyPresent);
            }
            ArtifactPresence::PresentDiffering { remote_checksum } => {
                bail!(
                    "{publisher}: '{}' already exists at {} with a different sha256 \
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
        .with_context(|| format!("{publisher}: failed to read '{}'", path.display()))?;

    log.verbose(&format!(
        "uploading {} ({} bytes) to {}",
        artifact.name(),
        body.len(),
        url
    ));

    // Validate the HTTP method up-front so the per-attempt send closure
    // can't see an unsupported value (and so a typo fails-fast outside
    // the retry loop, where it belongs — rebuilding the same Break error
    // on every attempt is wasted work).
    let method_upper = method.to_uppercase();
    match method_upper.as_str() {
        "PUT" | "POST" => {}
        other => bail!("{publisher}: unsupported HTTP method '{}'", other),
    }

    let label = format!("{publisher}: upload of '{}'", artifact.name());
    let art_name = artifact.name().to_string();
    let (status, _body) = retry_http_blocking_deadline(
        RetryLog::new(&label, log),
        policy,
        deadline,
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
            for (k, v) in rendered_headers {
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
                "{publisher}: upload of '{art_name}' failed: {method_upper} {status} — {detail}"
            )
        },
    )?;

    // Per-artifact upload-success detail is verbose-only; the entry summary
    // (default verbosity) reports the aggregate upload count.
    log.verbose(&format!("uploaded {} ({status}) → {url}", artifact.name()));
    Ok(UploadOutcome::Uploaded)
}

/// Decode Artifactory's `{"errors":[{"status":N,"message":"..."}]}` error
/// envelope into a human-readable string. Falls back to the raw body when
/// JSON decoding fails or the envelope shape doesn't match.
pub(crate) fn decode_artifactory_error_body(body: &str) -> String {
    // Defense-in-depth: if Artifactory echoes the Authorization header back
    // in the error envelope, scrub the token before it lands in the
    // user-visible log. Applied at the fallback / joined-output boundary so
    // redaction runs once regardless of which path produces the message.
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

        // Fail fast on a corrupt deb matrix-param slug before any bytes move
        // (and before dry-run, so a snapshot catches it too).
        validate_artifactory_deb_slugs(entry)?;

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

            // Log matching artifacts in dry-run (extra_files specs resolved so
            // the preview reflects the real upload set).
            let artifacts = collect_upload_artifacts_owned(
                ctx,
                "artifactory",
                mode,
                entry.ids.as_deref(),
                entry.exclude.as_deref(),
                entry.exts.as_deref(),
                CollectFlags {
                    checksum: include_checksum,
                    signature: include_signature,
                    meta: include_meta,
                    extra_files_only,
                },
                entry.extra_files.as_deref(),
                log,
            )?;
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            // Render per-artifact URLs through the same path live mode uses
            // so dry-run reflects template behaviour exactly.
            for a in &artifacts {
                let url = render_artifact_url(ctx, target_template, a, custom_artifact_name)?;
                let url = append_deb_matrix_params(&url, a, entry)?;
                log.status(&format!("(dry-run) {} ({}) → {}", a.name(), a.kind, url));
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

        // Collect artifacts (incl. resolved extra_files specs).
        let artifacts = collect_upload_artifacts_owned(
            ctx,
            "artifactory",
            mode,
            entry.ids.as_deref(),
            entry.exclude.as_deref(),
            entry.exts.as_deref(),
            CollectFlags {
                checksum: include_checksum,
                signature: include_signature,
                meta: include_meta,
                extra_files_only,
            },
            entry.extra_files.as_deref(),
            log,
        )?;

        if artifacts.is_empty() {
            // Distinguish a genuinely empty candidate set from an `exclude:`
            // glob that dropped everything (a typo silently uploading nothing).
            if entry.exclude.as_deref().is_some_and(|e| !e.is_empty()) {
                let pre_exclude = collect_upload_artifacts_owned(
                    ctx,
                    "artifactory",
                    mode,
                    entry.ids.as_deref(),
                    None,
                    entry.exts.as_deref(),
                    CollectFlags {
                        checksum: include_checksum,
                        signature: include_signature,
                        meta: include_meta,
                        extra_files_only,
                    },
                    entry.extra_files.as_deref(),
                    log,
                )
                .map(|v| v.len())
                .unwrap_or(0);
                if anodizer_core::artifact::exclude_filter_eliminated_all(
                    entry.exclude.as_deref(),
                    pre_exclude,
                    0,
                ) {
                    log.warn(&format!(
                        "exclude filter {:?} dropped all {} candidate artifact(s) for \
                         artifactory '{}'; check the globs match asset names, not full paths",
                        entry.exclude.as_deref().unwrap_or_default(),
                        pre_exclude,
                        name
                    ));
                }
            }
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

        // Upload each artifact through the shared HTTP-upload driver. The
        // only Artifactory-specific step is the Debian matrix-param append,
        // threaded in as the per-URL rewrite hook; everything else (render,
        // idempotency probe, retry, checksum/custom headers) is shared with
        // the generic `uploads:` publisher.
        let counts = crate::http_upload::upload_artifact_set(
            ctx,
            &client,
            target_template,
            &artifacts,
            &crate::http_upload::UploadEntryRequest {
                publisher: "artifactory",
                method,
                checksum_header,
                custom_headers,
                username: &username,
                password: &password,
                custom_artifact_name,
                overwrite,
            },
            &policy,
            ctx.options.parallelism,
            log,
            |url, artifact| append_deb_matrix_params(url, artifact, entry),
        )?;
        summary.uploaded += counts.uploaded;
        summary.already_present += counts.already_present;

        log.status(&crate::http_upload::upload_summary(
            counts.uploaded,
            counts.already_present,
            name,
        ));
    }

    Ok(summary)
}
