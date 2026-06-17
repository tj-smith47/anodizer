//! Generic HTTP upload publisher (`uploads:`).
//!
//! GoReleaser's "generic upload" pipe, mirrored: each `uploads:` entry PUTs
//! (or POSTs) the release artifacts to a templated target URL with optional
//! basic-auth, mTLS, a checksum header, and custom headers. It shares the
//! whole HTTP-upload core with the Artifactory publisher
//! ([`crate::http_upload::upload_artifact_set`] + the per-artifact helpers in
//! [`crate::artifactory`]); the only behavioural difference from Artifactory
//! is the absence of the JFrog Debian matrix-param append — a generic
//! endpoint receives the rendered URL verbatim.

use anodizer_core::config::UploadConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;

use crate::artifactory::{
    ArtifactoryTarget, CollectFlags, build_reqwest_client, collect_target_artifacts_best_effort,
    collect_upload_artifacts_owned, render_artifact_url, validate_upload_mode_for,
};

/// Tally of what a generic-uploads publish run did, so the caller can decide
/// whether the whole run was an idempotent no-op (everything skipped) versus a
/// real publish (at least one upload).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadsSummary {
    /// Artifacts PUT/POSTed this run (freshly uploaded or overwritten).
    pub uploaded: usize,
    /// Artifacts skipped because an identical copy already existed.
    pub already_present: usize,
}

impl UploadsSummary {
    /// True when at least one artifact was considered AND every one was an
    /// idempotent skip — the signal the publisher uses to record
    /// `Skipped(AlreadyPublished)` instead of `Succeeded`.
    pub fn is_fully_idempotent_skip(&self) -> bool {
        self.uploaded == 0 && self.already_present > 0
    }
}

/// Default checksum header for generic uploads — GoReleaser uses
/// `X-Checksum-Sha256` for both Artifactory and the generic upload pipe.
const DEFAULT_CHECKSUM_HEADER: &str = "X-Checksum-Sha256";

/// Resolve the active collect-flags for one upload entry.
fn entry_collect_flags(entry: &UploadConfig) -> CollectFlags {
    CollectFlags {
        checksum: entry.checksum.unwrap_or(false),
        signature: entry.signature.unwrap_or(false),
        meta: entry.meta.unwrap_or(false),
        extra_files_only: entry.extra_files_only.unwrap_or(false),
    }
}

/// Upload release artifacts to one or more generic HTTP endpoints via the
/// shared HTTP-PUT/POST machinery.
///
/// This is a top-level publisher: it reads from `ctx.config.uploads` rather
/// than from per-crate publish configs. Each entry specifies a target URL
/// template, optional credentials, artifact filters, and the
/// checksum/signature/meta inclusion toggles. Mirrors GoReleaser's generic
/// upload pipe; the per-artifact upload loop is shared with Artifactory.
pub fn publish_uploads(ctx: &Context, log: &StageLogger) -> Result<UploadsSummary> {
    let mut summary = UploadsSummary::default();
    let entries = match ctx.config.uploads {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(summary),
    };

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every entry's per-artifact upload.
    let policy = ctx.retry_policy();

    for entry in entries {
        let label = format!(
            "uploads entry '{}'",
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

        // Name is required (it keys the credential env cascade and dry-run
        // diagnostics).
        let name = match entry.name {
            Some(ref n) if !n.is_empty() => n.as_str(),
            _ => bail!("uploads: entry is missing required 'name' field"),
        };

        // Validate mode (default: "archive").
        let mode = entry.mode.as_deref().unwrap_or("archive");
        validate_upload_mode_for("uploads", mode)?;

        // Target URL is required.
        let target_template = match entry.target {
            ref t if !t.is_empty() => t.as_str(),
            _ => bail!("uploads: entry '{}' is missing required 'target' URL", name),
        };

        // HTTP method (default: PUT).
        let method = entry.method.as_deref().unwrap_or("PUT");

        // Credential cascade lives in http_upload::resolve_http_credentials so
        // artifactory + uploads share one implementation. `anonymous_ok = true`
        // because generic endpoints (public mirrors, pre-signed URLs) may not
        // need basic-auth; only the half-set state is refused.
        let (username, password) = crate::http_upload::resolve_http_credentials(
            ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "uploads",
                entry_name: name,
                config_username: entry.username.as_deref(),
                config_password: entry.password.as_deref(),
                env_prefix: "UPLOAD",
                anonymous_ok: true,
            },
        )?;
        let name_upper = name.to_uppercase().replace('-', "_");
        let named_env_var = format!("UPLOAD_{}_SECRET", name_upper);

        // Determine checksum header name (default: X-Checksum-Sha256).
        let checksum_header = entry
            .checksum_header
            .as_deref()
            .unwrap_or(DEFAULT_CHECKSUM_HEADER);

        // Collect custom headers.
        let empty = HashMap::new();
        let custom_headers = entry.custom_headers.as_ref().unwrap_or(&empty);

        // Include flags
        let include_checksum = entry.checksum.unwrap_or(false);
        let include_signature = entry.signature.unwrap_or(false);
        let include_meta = entry.meta.unwrap_or(false);
        let custom_artifact_name = entry.custom_artifact_name.unwrap_or(false);
        let flags = entry_collect_flags(entry);

        // --- Dry-run logging (no network) ---
        if ctx.is_dry_run() {
            let target_url = ctx
                .render_template(target_template)
                .with_context(|| format!("uploads: failed to render target URL for '{}'", name))?;
            log.status(&format!(
                "(dry-run) would upload artifacts to '{}' at {} (mode={}, method={}, user={})",
                name,
                log.redact(&target_url),
                mode,
                method,
                username
            ));
            if !custom_headers.is_empty() {
                for (k, v) in custom_headers {
                    let rendered_v = crate::util::render_or_warn(ctx, log, "uploads.headers", v)?;
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

            let artifacts = collect_upload_artifacts_owned(
                ctx,
                "uploads",
                mode,
                entry.ids.as_deref(),
                entry.exts.as_deref(),
                flags,
                entry.extra_files.as_deref(),
                log,
            )?;
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            // Render per-artifact URLs through the same path live mode uses so
            // dry-run reflects template behaviour exactly.
            for a in &artifacts {
                let url = render_artifact_url(ctx, target_template, a, custom_artifact_name)?;
                log.status(&format!(
                    "(dry-run) {} ({}) → {} {}",
                    a.name(),
                    a.kind,
                    method.to_uppercase(),
                    url
                ));
            }
            continue;
        }

        // --- Live mode ---
        crate::http_upload::validate_mtls_pair(
            "uploads",
            name,
            entry.client_x509_cert.as_deref(),
            entry.client_x509_key.as_deref(),
        )?;

        let client = build_reqwest_client(
            entry.client_x509_cert.as_deref(),
            entry.client_x509_key.as_deref(),
            entry.trusted_certificates.as_deref(),
        )?;

        let artifacts = collect_upload_artifacts_owned(
            ctx,
            "uploads",
            mode,
            entry.ids.as_deref(),
            entry.exts.as_deref(),
            flags,
            entry.extra_files.as_deref(),
            log,
        )?;

        if artifacts.is_empty() {
            log.status(&format!(
                "no matching upload artifacts for '{}' (mode={})",
                name, mode
            ));
            continue;
        }

        log.status(&format!(
            "uploading {} artifacts to '{}' (mode={})",
            artifacts.len(),
            name,
            mode
        ));

        let overwrite = entry.overwrite.unwrap_or(false);

        // Generic uploads send the rendered URL verbatim — no Debian
        // matrix-param append (that is an Artifactory-only concern), so the
        // shared driver's rewrite hook is the identity.
        let counts = crate::http_upload::upload_artifact_set(
            ctx,
            &client,
            target_template,
            &artifacts,
            &crate::http_upload::UploadEntryRequest {
                publisher: "uploads",
                method,
                checksum_header,
                custom_headers,
                username: &username,
                password: &password,
                custom_artifact_name,
                overwrite,
            },
            &policy,
            log,
            |url, _artifact| Ok(url.to_string()),
        )?;
        summary.uploaded += counts.uploaded;
        summary.already_present += counts.already_present;

        log.status(&format!("upload complete for '{}'", name));
    }

    Ok(summary)
}

/// Re-walk the configured `uploads:` entries to produce the fully rendered
/// upload URLs that [`publish_uploads`] would PUT/POST to. Drives the
/// [`Publisher`](anodizer_core::Publisher) wrapper's rollback evidence so a
/// later rollback can DELETE each URL using the same credential resolution the
/// publish path used.
///
/// Best-effort: entries that hit a render or filter error are silently
/// skipped, since failures here only narrow the rollback checklist (the
/// publish path's own error handling has already surfaced any blocker).
pub(crate) fn collect_upload_targets(ctx: &Context) -> Vec<ArtifactoryTarget> {
    let mut out: Vec<ArtifactoryTarget> = Vec::new();
    let entries = match ctx.config.uploads.as_ref() {
        Some(v) if !v.is_empty() => v,
        _ => return out,
    };
    for entry in entries {
        // Skip evaluation must match publish_uploads's behaviour so a skipped
        // entry doesn't leak phantom rollback targets.
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
        if entry.target.is_empty() {
            continue;
        }
        let target_template = entry.target.as_str();
        let mode = entry.mode.as_deref().unwrap_or("archive");
        let custom_artifact_name = entry.custom_artifact_name.unwrap_or(false);
        let artifacts = collect_target_artifacts_best_effort(
            ctx,
            "uploads",
            mode,
            entry.ids.as_deref(),
            entry.exts.as_deref(),
            entry_collect_flags(entry),
            entry.extra_files.as_deref(),
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

// ---------------------------------------------------------------------------
// UploadsPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_uploads`] in the [`anodizer_core::Publisher`] trait so the
// dispatch path drives generic HTTP uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable bytes,
// server-side deletable). `required = false` (GoReleaser's generic upload is
// non-required; a failure warns rather than aborting the release unless an
// entry opts in with `required: true`).
//
// Rollback shape: per uploaded URL, issue an HTTP DELETE with the same
// credential cascade `publish_uploads` uses (basic auth from `username` +
// `password` plus the per-entry `UPLOAD_<NAME>_{USERNAME,SECRET}` override).
simple_publisher!(
    UploadsPublisher,
    "uploads",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("UPLOAD_<NAME>_SECRET delete"),
);

impl anodizer_core::Publisher for UploadsPublisher {
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
        // Mirrors `resolve_http_credentials` (anonymous_ok = true): per entry,
        // each of username/password comes from the templated config value or
        // the `UPLOAD_<NAME>_{USERNAME,SECRET}` env pair. Anonymous entries
        // (neither config value set) demand nothing.
        let mut out = Vec::new();
        for entry in ctx.config.uploads.iter().flatten() {
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
                &format!("UPLOAD_{}_USERNAME", name_upper),
            ) {
                out.push(req);
            }
            if let Some(req) = crate::publisher_helpers::secret_requirement(
                entry.password.as_deref(),
                &format!("UPLOAD_{}_SECRET", name_upper),
            ) {
                out.push(req);
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let summary = publish_uploads(ctx, &log)?;
        // Every matched artifact was already present at its target path (an
        // idempotent re-run): record a SKIP, not a fresh publish.
        if summary.is_fully_idempotent_skip() {
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
                anodizer_core::SkipReason::AlreadyPublished,
            ));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("uploads");
        let targets = collect_upload_targets(ctx);
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(first.url.clone());
        }
        evidence.artifact_paths = targets
            .iter()
            .map(|t| std::path::PathBuf::from(&t.url))
            .collect();
        evidence.extra = crate::artifactory::encode_artifactory_targets(&targets);
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
                "uploads",
                "upload URLs",
            ));
            return Ok(());
        }
        let structured = crate::artifactory::decode_artifactory_targets(&evidence.extra);
        let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "uploads rollback failed to build HTTP client: {}; manual cleanup required",
                    e
                ));
                return Ok(());
            }
        };

        let by_url: std::collections::HashMap<String, String> = structured
            .iter()
            .map(|t| (t.url.clone(), t.entry.clone()))
            .collect();

        let mut deleted = 0usize;
        let mut already_absent = 0usize;
        let mut failed = 0usize;
        for p in &evidence.artifact_paths {
            let url = p.display().to_string();
            let basic_auth = by_url
                .get(&url)
                .and_then(|entry| resolve_rollback_credentials(ctx, entry))
                .filter(|(u, pw)| !u.is_empty() && !pw.is_empty());
            log.status(&format!("DELETE {}", url));
            let mut req = client.delete(&url);
            if let Some((ref u, ref pw)) = basic_auth {
                req = req.basic_auth(u, Some(pw));
            }
            match req.send() {
                Ok(resp) => {
                    let status = resp.status();
                    match crate::artifactory::classify_delete_status(status) {
                        crate::artifactory::DeleteOutcome::Deleted => deleted += 1,
                        crate::artifactory::DeleteOutcome::AlreadyAbsent => {
                            already_absent += 1;
                            log.status(&format!(
                                "DELETE {} returned HTTP {} (already absent)",
                                url, status
                            ));
                        }
                        crate::artifactory::DeleteOutcome::Failed(_) => {
                            failed += 1;
                            log.warn(&format!(
                                "DELETE {} returned HTTP {} (manual cleanup may be required)",
                                url, status
                            ));
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    log.warn(&format!(
                        "DELETE {} transport error: {} (manual cleanup may be required)",
                        url, e
                    ));
                }
            }
        }
        log.status(&format!(
            "uploads rollback deleted {} artifact(s), {} already absent, {} failure(s)",
            deleted, already_absent, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn skips_on_nightly(&self) -> bool {
        // Versioned upload paths don't clobber stable content; nightly
        // re-uploads are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}

/// Resolve `(username, password)` for a generic upload entry at rollback time,
/// mirroring the exact credential cascade `publish_uploads` uses (config →
/// `UPLOAD_<NAME>_USERNAME` / `UPLOAD_<NAME>_SECRET` env). Returns `None` when
/// the entry is no longer present in config (operator pruned the YAML between
/// publish and rollback) so the caller falls back to an unauthenticated
/// DELETE (which surfaces a 401 in the failure bucket rather than bailing).
fn resolve_rollback_credentials(ctx: &Context, entry_name: &str) -> Option<(String, String)> {
    let entries = ctx.config.uploads.as_ref()?;
    let entry = entries
        .iter()
        .find(|e| e.name.as_deref() == Some(entry_name))?;
    crate::http_upload::resolve_http_credentials(
        ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "uploads",
            entry_name,
            config_username: entry.username.as_deref(),
            config_password: entry.password.as_deref(),
            env_prefix: "UPLOAD",
            anonymous_ok: true,
        },
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, StringOrBool, UploadConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::LogCapture;
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

    /// Build a dry-run context with `Version`/`Tag`/`ProjectName` seeded so
    /// `{{ .Version }}`-style target templates render.
    fn dry_run_ctx_versioned(config: Config) -> Context {
        let mut ctx = dry_run_ctx(config);
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx
    }

    fn add_archive(ctx: &mut Context, name: &str, target: Option<&str>) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: name.to_string(),
            path: PathBuf::from(format!("dist/{name}")),
            target: target.map(str::to_string),
            crate_name: "anodizer".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    fn add_kind(ctx: &mut Context, kind: ArtifactKind, name: &str) {
        ctx.artifacts.add(Artifact {
            kind,
            name: name.to_string(),
            path: PathBuf::from(format!("dist/{name}")),
            target: None,
            crate_name: "anodizer".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    fn capture_dry_run(ctx: &Context) -> Vec<String> {
        let capture = LogCapture::new();
        let log = ctx.logger("uploads").with_capture_handle(capture.clone());
        publish_uploads(ctx, &log).expect("dry-run publish_uploads should succeed");
        capture.all_messages().into_iter().map(|(_, m)| m).collect()
    }

    #[test]
    fn skips_when_no_config() {
        let ctx = dry_run_ctx(Config::default());
        let log = ctx.logger("uploads");
        assert!(publish_uploads(&ctx, &log).is_ok());
    }

    #[test]
    fn skips_when_empty_vec() {
        let mut config = Config::default();
        config.uploads = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("uploads");
        assert!(publish_uploads(&ctx, &log).is_ok());
    }

    #[test]
    fn skips_when_skip_true() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("uploads");
        assert!(publish_uploads(&ctx, &log).is_ok());
    }

    #[test]
    fn requires_name() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: None,
            target: "https://uploads.example.com/".to_string(),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("uploads");
        let err = publish_uploads(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("missing required 'name'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn requires_target() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("mirror".to_string()),
            target: String::new(),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("uploads");
        let err = publish_uploads(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("missing required 'target'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn invalid_mode_errors_with_uploads_label() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            mode: Some("bogus".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("uploads");
        let err = publish_uploads(&ctx, &log).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("uploads: invalid upload mode 'bogus'"),
            "{msg}"
        );
    }

    /// Half-set credentials (password without username) must hard-error even
    /// though `uploads` tolerates fully-anonymous endpoints — a half-set pair
    /// is always a config bug, never an intentional anonymous upload. Live
    /// mode only; dry-run skips credential validation. No artifacts are
    /// registered so the loop reaches credential resolution and bails there.
    #[test]
    fn half_set_credentials_error_in_live_mode() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            password: Some("s3cr3t".to_string()),
            ..Default::default()
        }]);
        // NOT dry-run: credential resolution enforces the pair invariant.
        let ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("uploads");
        let err = publish_uploads(&ctx, &log).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("username set") || msg.contains("password set"),
            "expected half-set credential error, got: {msg}"
        );
    }

    /// Fully-anonymous endpoints are allowed (no username/password, no env):
    /// the credential cascade must NOT bail (`anonymous_ok = true`). With no
    /// matching artifacts the run is a clean no-op.
    #[test]
    fn anonymous_endpoint_is_allowed() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("public-mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            ..Default::default()
        }]);
        let ctx = Context::new(config, ContextOptions::default());
        let log = ctx.logger("uploads");
        let summary = publish_uploads(&ctx, &log).expect("anonymous upload allowed");
        assert_eq!(summary.uploaded, 0);
    }

    #[test]
    fn dry_run_logs_rendered_target_and_method() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("jarvispro".to_string()),
            target: "https://uploads.jarvispro.io/anodizer/{{ .Version }}/".to_string(),
            method: Some("PUT".to_string()),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx_versioned(config);
        add_archive(
            &mut ctx,
            "anodizer-1.2.3-linux.tar.gz",
            Some("x86_64-unknown-linux-gnu"),
        );
        let msgs = capture_dry_run(&ctx);
        let joined = msgs.join("\n");
        // Target template rendered with Version; per-artifact line shows the
        // PUT and the appended artifact name (custom_artifact_name=false).
        assert!(
            joined.contains("https://uploads.jarvispro.io/anodizer/1.2.3/"),
            "rendered target missing:\n{joined}"
        );
        assert!(
            joined.contains(
                "anodizer-1.2.3-linux.tar.gz (archive) → PUT \
                 https://uploads.jarvispro.io/anodizer/1.2.3/anodizer-1.2.3-linux.tar.gz"
            ),
            "per-artifact PUT line missing:\n{joined}"
        );
    }

    #[test]
    fn dry_run_honors_post_method() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("poster".to_string()),
            target: "https://uploads.example.com/{{ .Tag }}/".to_string(),
            method: Some("POST".to_string()),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx_versioned(config);
        add_archive(&mut ctx, "app.tar.gz", None);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(joined.contains("method=POST"), "{joined}");
        assert!(
            joined.contains("→ POST https://uploads.example.com/v1.2.3/app.tar.gz"),
            "{joined}"
        );
    }

    #[test]
    fn dry_run_logs_custom_headers_and_checksum_header() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Anodizer-Tag".to_string(), "{{ .Tag }}".to_string());
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("hdr".to_string()),
            target: "https://uploads.example.com/".to_string(),
            custom_headers: Some(headers),
            checksum_header: Some("X-My-Sum".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx_versioned(config);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(
            joined.contains("would send custom header X-Anodizer-Tag=v1.2.3"),
            "{joined}"
        );
        assert!(
            joined.contains("would send checksum header X-My-Sum"),
            "{joined}"
        );
    }

    #[test]
    fn dry_run_default_checksum_header() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("dflt".to_string()),
            target: "https://uploads.example.com/".to_string(),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx_versioned(config);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(
            joined.contains("would send checksum header X-Checksum-Sha256"),
            "{joined}"
        );
    }

    /// `mode`/`ids`/`exts`/`checksum`/`signature` selection: archive mode with
    /// an ext filter selects only matching archives; `checksum: true` and
    /// `signature: true` pull in the sidecars. Asserted through the dry-run
    /// per-artifact lines, which exercise the live selection path.
    #[test]
    fn dry_run_selects_by_mode_exts_and_sidecars() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("sel".to_string()),
            target: "https://uploads.example.com/".to_string(),
            exts: Some(vec!["tar.gz".to_string(), "sha256".to_string()]),
            checksum: Some(true),
            signature: Some(true),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx_versioned(config);
        add_archive(&mut ctx, "app.tar.gz", None);
        add_archive(&mut ctx, "app.zip", None); // excluded by ext filter
        add_kind(&mut ctx, ArtifactKind::Checksum, "app.tar.gz.sha256");
        add_kind(&mut ctx, ArtifactKind::Signature, "app.tar.gz.sig");
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(joined.contains("app.tar.gz (archive) →"), "{joined}");
        assert!(
            !joined.contains("app.zip (archive) →"),
            "zip should be excluded:\n{joined}"
        );
        // checksum + signature sidecars pulled in by the include flags.
        assert!(
            joined.contains("app.tar.gz.sha256"),
            "checksum sidecar missing:\n{joined}"
        );
        assert!(
            joined.contains("app.tar.gz.sig"),
            "signature sidecar missing:\n{joined}"
        );
    }

    /// Per-crate target rendering: a workspace crate's `target` template that
    /// references `{{ .Version }}` and `{{ .ProjectName }}` renders with the
    /// per-crate values seeded into the context.
    #[test]
    fn per_crate_target_rendering() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("pc".to_string()),
            target: "https://uploads.example.com/{{ .ProjectName }}/{{ .Version }}/".to_string(),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx(config);
        // Simulate a per-crate render pass: the dispatch loop reseeds these
        // per published crate before invoking the stage.
        ctx.template_vars_mut().set("ProjectName", "core-crate");
        ctx.template_vars_mut().set("Version", "0.5.0");
        add_archive(&mut ctx, "core-crate-0.5.0.tar.gz", None);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(
            joined.contains("https://uploads.example.com/core-crate/0.5.0/core-crate-0.5.0.tar.gz"),
            "per-crate rendered target missing:\n{joined}"
        );
    }

    /// An `extra_files` glob spec resolves to an uploadable artifact and is
    /// appended to the normal upload set (GoReleaser's `extrafiles.Find`
    /// behaviour). Proven through the dry-run per-artifact lines.
    #[test]
    fn extra_files_spec_resolves_and_uploads() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("LICENSE.txt"), b"MIT").unwrap();
        let glob = format!("{}/LICENSE.txt", tmp.path().display());

        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("ef".to_string()),
            target: "https://uploads.example.com/".to_string(),
            extra_files: Some(vec![anodizer_core::config::ExtraFileSpec::Glob(glob)]),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx_versioned(config);
        add_archive(&mut ctx, "app.tar.gz", None);
        let joined = capture_dry_run(&ctx).join("\n");
        // The normal archive AND the resolved extra file both appear.
        assert!(joined.contains("app.tar.gz (archive) →"), "{joined}");
        assert!(
            joined.contains(
                "LICENSE.txt (uploadable_file) → PUT https://uploads.example.com/LICENSE.txt"
            ),
            "resolved extra_file missing:\n{joined}"
        );
        assert!(joined.contains("2 artifacts matched"), "{joined}");
    }

    /// `extra_files_only: true` uploads ONLY the resolved extra files — the
    /// normal mode-filtered release set is skipped.
    #[test]
    fn extra_files_only_uploads_only_extra_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("notes.md"), b"notes").unwrap();
        let glob = format!("{}/notes.md", tmp.path().display());

        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("efo".to_string()),
            target: "https://uploads.example.com/".to_string(),
            extra_files: Some(vec![anodizer_core::config::ExtraFileSpec::Glob(glob)]),
            extra_files_only: Some(true),
            ..Default::default()
        }]);
        let mut ctx = dry_run_ctx_versioned(config);
        // This archive must NOT be uploaded under extra_files_only.
        add_archive(&mut ctx, "app.tar.gz", None);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(
            joined.contains("notes.md (uploadable_file) →"),
            "resolved extra_file missing:\n{joined}"
        );
        assert!(
            !joined.contains("app.tar.gz (archive) →"),
            "archive should be excluded under extra_files_only:\n{joined}"
        );
        assert!(joined.contains("1 artifacts matched"), "{joined}");
    }

    /// A glob matching multiple files expands to one artifact per match, and a
    /// `name_template` override renders against the context template vars.
    #[test]
    fn extra_files_glob_expansion_and_name_template() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        let multi_glob = format!("{}/*.bin", tmp.path().display());

        // Multi-match glob → two artifacts (no name_template).
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("glob".to_string()),
            target: "https://uploads.example.com/".to_string(),
            extra_files: Some(vec![anodizer_core::config::ExtraFileSpec::Glob(multi_glob)]),
            extra_files_only: Some(true),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx_versioned(config);
        let joined = capture_dry_run(&ctx).join("\n");
        assert!(joined.contains("a.bin (uploadable_file) →"), "{joined}");
        assert!(joined.contains("b.bin (uploadable_file) →"), "{joined}");
        assert!(joined.contains("2 artifacts matched"), "{joined}");

        // name_template override renders against {{ .ProjectName }}.
        std::fs::write(tmp.path().join("single.bin"), b"x").unwrap();
        let one_glob = format!("{}/single.bin", tmp.path().display());
        let mut config2 = Config::default();
        config2.uploads = Some(vec![UploadConfig {
            name: Some("nt".to_string()),
            target: "https://uploads.example.com/".to_string(),
            extra_files: Some(vec![anodizer_core::config::ExtraFileSpec::Detailed {
                glob: one_glob,
                name_template: Some("{{ .ProjectName }}-renamed.bin".to_string()),
                allow_empty: false,
            }]),
            extra_files_only: Some(true),
            ..Default::default()
        }]);
        let ctx2 = dry_run_ctx_versioned(config2);
        let joined2 = capture_dry_run(&ctx2).join("\n");
        assert!(
            joined2.contains(
                "anodizer-renamed.bin (uploadable_file) → PUT \
                 https://uploads.example.com/anodizer-renamed.bin"
            ),
            "name_template override missing:\n{joined2}"
        );
    }

    /// `UPLOAD_<NAME>_USERNAME`/`_SECRET` env cascade resolves credentials when
    /// the config leaves them unset (live mode, so the pair is enforced).
    #[test]
    fn credentials_resolve_from_named_env() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("my-mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            ..Default::default()
        }]);
        let mut ctx = Context::new(config, ContextOptions::default());
        // Hyphen in the entry name maps to '_' in the env-var key.
        ctx.template_vars_mut()
            .set_env("UPLOAD_MY_MIRROR_USERNAME", "deployer");
        ctx.template_vars_mut()
            .set_env("UPLOAD_MY_MIRROR_SECRET", "tok");
        let entry = ctx.config.uploads.as_ref().unwrap()[0].clone();
        let (u, p) = crate::http_upload::resolve_http_credentials(
            &ctx,
            &crate::http_upload::CredentialResolveSpec {
                publisher: "uploads",
                entry_name: entry.name.as_deref().unwrap(),
                config_username: entry.username.as_deref(),
                config_password: entry.password.as_deref(),
                env_prefix: "UPLOAD",
                anonymous_ok: true,
            },
        )
        .unwrap();
        assert_eq!(u, "deployer");
        assert_eq!(p, "tok");
    }

    /// Registration smoke test: a configured `uploads:` block makes the
    /// `UploadsPublisher` appear in `configured_publishers`, proving the dead
    /// config now drives a real stage.
    #[test]
    fn registered_when_configured() {
        let mut config = Config::default();
        config.uploads = Some(vec![UploadConfig {
            name: Some("mirror".to_string()),
            target: "https://uploads.example.com/".to_string(),
            ..Default::default()
        }]);
        let ctx = Context::new(config, ContextOptions::default());
        let names: Vec<String> = crate::registry::configured_publishers(&ctx)
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "uploads"),
            "publishers: {names:?}"
        );
    }

    #[test]
    fn not_registered_when_absent() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let names: Vec<String> = crate::registry::configured_publishers(&ctx)
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n == "uploads"),
            "publishers: {names:?}"
        );
    }

    #[test]
    fn idempotent_skip_summary() {
        let s = UploadsSummary {
            uploaded: 0,
            already_present: 3,
        };
        assert!(s.is_fully_idempotent_skip());
        let s2 = UploadsSummary {
            uploaded: 1,
            already_present: 2,
        };
        assert!(!s2.is_fully_idempotent_skip());
    }
}

/// Live-mode (`dry_run: false`) HTTP coverage for `publish_uploads`. Every
/// test points the per-entry `target:` URL at an in-process scripted
/// responder (an ephemeral `127.0.0.1:0` port) and asserts on the recorded
/// traffic — method, path, request body, and the credential / checksum
/// headers (now observable via the responder's header capture). The
/// generic-upload PUT/POST, basic-auth header construction, checksum header,
/// success, and non-2xx → contextful-error mapping run through the real
/// `upload_artifact_set` → `artifactory::upload_single_artifact` core that
/// `publish_uploads` drives.
///
/// Credentials are injected through a per-context [`MapEnvSource`] (NOT the
/// process env), so no test mutates `std::env` and none needs a serial
/// guard — they bind distinct ephemeral ports and run fully in parallel.
#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod live_http_tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, HumanDuration, RetryConfig, UploadConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::hashing::sha256_file;
    use anodizer_core::test_helpers::scripted_responder::{
        RequestLog, ScriptedRoute, spawn_scripted_responder_on,
    };
    use std::net::SocketAddr;
    use std::time::Duration;

    /// 2-attempt, 1ms-backoff retry so a 5xx-path test doesn't inherit the
    /// 10-attempt / 10s production default and stall the suite.
    fn fast_retry() -> RetryConfig {
        RetryConfig {
            attempts: 2,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(2)),
        }
    }

    /// Build a live (non-dry-run) single-archive upload context whose only
    /// `uploads:` entry targets `http://{addr}/repo/` (so the artifact PUTs
    /// to `/repo/<name>`). `mutate` customizes the entry (method, auth,
    /// custom headers, …). Returns the backing `TempDir` (bind it for the
    /// duration of the test so its `Drop` cleans up the artifact), the
    /// context, the file's hex sha256 (for scripting a matching HEAD probe),
    /// and the artifact name.
    fn live_ctx(
        addr: SocketAddr,
        env: MapEnvSource,
        mutate: impl FnOnce(&mut UploadConfig),
    ) -> (tempfile::TempDir, Context, String, &'static str) {
        let art_name = "app-1.0.0.tar.gz";
        let dir = tempfile::tempdir().unwrap();
        let art_path = dir.path().join(art_name);
        std::fs::write(&art_path, b"upload-bytes").unwrap();
        let checksum = sha256_file(&art_path).unwrap();

        let mut entry = UploadConfig {
            name: Some("jarvispro".to_string()),
            target: format!("http://{addr}/repo/"),
            ..Default::default()
        };
        mutate(&mut entry);

        let mut config = Config::default();
        config.project_name = "app".to_string();
        config.retry = Some(fast_retry());
        config.uploads = Some(vec![entry]);

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.set_env_source(env);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: art_name.to_string(),
            path: art_path,
            target: None,
            crate_name: "app".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
        (dir, ctx, checksum, art_name)
    }

    fn count_calls(log: &[RequestLog], method: &str, path: &str) -> usize {
        log.iter()
            .filter(|e| e.method == method && e.path == path)
            .count()
    }

    /// Default method (PUT) with basic-auth from `UPLOAD_<NAME>_SECRET`:
    /// the absence probe (HEAD) returns 404, then the PUT lands the bytes
    /// carrying the `Authorization: Basic …` header and the default
    /// `X-Checksum-Sha256` header. Proves the request construction +
    /// success path end to end.
    #[test]
    fn live_put_with_basic_auth_and_checksum_header() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| {
            vec![
                ScriptedRoute {
                    method: "HEAD",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
                ScriptedRoute {
                    method: "PUT",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
            ]
        });

        let env = MapEnvSource::new()
            .with("UPLOAD_JARVISPRO_USERNAME", "ci-bot")
            .with("UPLOAD_JARVISPRO_SECRET", "s3cr3t-pat");
        let (_dir, ctx, checksum, _name) = live_ctx(addr, env, |_| {});
        let log_handle = ctx.logger("uploads");
        let summary = publish_uploads(&ctx, &log_handle).expect("live PUT upload succeeds");
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.already_present, 0);

        let entries = log.lock().unwrap();
        assert_eq!(
            count_calls(&entries, "HEAD", "/repo/app-1.0.0.tar.gz"),
            1,
            "absence probe runs first (overwrite unset): {entries:?}"
        );
        let put = entries
            .iter()
            .find(|e| e.method == "PUT")
            .expect("PUT recorded");
        assert_eq!(put.path, "/repo/app-1.0.0.tar.gz");
        assert_eq!(put.body, "upload-bytes", "the file bytes are the PUT body");
        // Basic auth: base64("ci-bot:s3cr3t-pat").
        let expected = {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("ci-bot:s3cr3t-pat")
            )
        };
        assert_eq!(
            put.header("Authorization"),
            Some(expected.as_str()),
            "PUT carries the resolved basic-auth header: {:?}",
            put.headers
        );
        assert_eq!(
            put.header("X-Checksum-Sha256"),
            Some(checksum.as_str()),
            "PUT carries the default checksum header: {:?}",
            put.headers
        );
    }

    /// `method: POST` + a custom checksum header name + a custom header are
    /// all honored: the artifact is POSTed (not PUT) and the configured
    /// header names reach the wire.
    #[test]
    fn live_post_method_and_custom_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| {
            vec![
                ScriptedRoute {
                    method: "HEAD",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
            ]
        });

        let (_dir, ctx, _checksum, _name) = live_ctx(addr, MapEnvSource::new(), |e| {
            e.method = Some("POST".to_string());
            e.checksum_header = Some("X-My-Digest".to_string());
            let mut h = std::collections::HashMap::new();
            h.insert("X-Trace-Id".to_string(), "abc-123".to_string());
            e.custom_headers = Some(h);
        });
        let log_handle = ctx.logger("uploads");
        let summary = publish_uploads(&ctx, &log_handle).expect("live POST upload succeeds");
        assert_eq!(summary.uploaded, 1);

        let entries = log.lock().unwrap();
        let post = entries
            .iter()
            .find(|e| e.method == "POST")
            .expect("POST recorded");
        assert!(
            post.header("X-My-Digest").is_some(),
            "custom checksum header name honored: {:?}",
            post.headers
        );
        assert_eq!(
            post.header("X-Trace-Id"),
            Some("abc-123"),
            "custom header reaches the wire: {:?}",
            post.headers
        );
        // No basic-auth header: anonymous endpoint (no creds configured).
        assert!(
            post.header("Authorization").is_none(),
            "anonymous endpoint sends no auth: {:?}",
            post.headers
        );
    }

    /// A non-2xx PUT response maps to a contextful error naming the failing
    /// artifact / status; the publish fails rather than silently succeeding.
    #[test]
    fn live_put_5xx_maps_to_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (_addr, _log) = spawn_scripted_responder_on(listener, |_| {
            vec![
                ScriptedRoute {
                    method: "HEAD",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
                ScriptedRoute {
                    method: "PUT",
                    path_pattern: "/repo/app-1.0.0.tar.gz",
                    // Always 500: the fast retry exhausts, then the error
                    // surfaces.
                    response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nboom!",
                    times: None,
                },
            ]
        });

        let env = MapEnvSource::new()
            .with("UPLOAD_JARVISPRO_USERNAME", "u")
            .with("UPLOAD_JARVISPRO_SECRET", "p");
        let (_dir, ctx, _checksum, _name) = live_ctx(addr, env, |_| {});
        let log_handle = ctx.logger("uploads");
        let err = publish_uploads(&ctx, &log_handle).expect_err("5xx PUT must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("app-1.0.0.tar.gz") || chain.contains("500") || chain.contains("upload"),
            "error names the failing upload / status: {chain}"
        );
    }

    /// A templated `target:` URL (`{{ .Version }}` — the workspace / per-crate
    /// path where each crate's rendered Version threads into the upload URL)
    /// resolves before the request: the PUT routes to `/repo/1.0.0/app-…`.
    #[test]
    fn live_templated_target_renders_version_into_path() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (_addr, log) = spawn_scripted_responder_on(listener, |_| {
            vec![
                ScriptedRoute {
                    method: "HEAD",
                    path_pattern: "/repo/1.0.0/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
                ScriptedRoute {
                    method: "PUT",
                    path_pattern: "/repo/1.0.0/app-1.0.0.tar.gz",
                    response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
                    times: None,
                },
            ]
        });

        let (_dir, ctx, _checksum, _name) = live_ctx(addr, MapEnvSource::new(), |e| {
            e.target = format!("http://{addr}/repo/{{{{ .Version }}}}/");
        });
        let log_handle = ctx.logger("uploads");
        let summary = publish_uploads(&ctx, &log_handle).expect("templated target upload succeeds");
        assert_eq!(summary.uploaded, 1);

        let entries = log.lock().unwrap();
        assert_eq!(
            count_calls(&entries, "PUT", "/repo/1.0.0/app-1.0.0.tar.gz"),
            1,
            "PUT routed to the rendered (Version-substituted) target: {entries:?}"
        );
    }
}
