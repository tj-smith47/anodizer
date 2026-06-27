//! Shared helpers for HTTP-based publishers (Artifactory + generic uploads).
//!
//! Both publishers walk the same per-entry credential cascade
//! (config → env), the same mTLS pair-check, and the same anonymous-upload
//! refusal pattern. The patterns used to be open-coded twice; this module
//! is the single source of truth so a fix in one place reaches both.

use anodizer_core::artifact::Artifact;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::parallel::run_parallel_chunks;
use anodizer_core::retry::RetryPolicy;
use anyhow::{Context as _, Result, bail};

use crate::artifactory::{UploadAuth, UploadHeaders, UploadOutcome, render_artifact_url};

/// Caller spec for `resolve_http_credentials`. Captures the small handful
/// of per-publisher knobs that distinguish artifactory's refusal of
/// anonymous uploads from upload's tolerance for them, plus the env-var
/// prefix used to look up secrets.
pub(crate) struct CredentialResolveSpec<'a> {
    /// Publisher label used in error messages ("artifactory" / "upload").
    pub publisher: &'a str,
    /// Entry name; appears in error messages and joined into env-var keys.
    pub entry_name: &'a str,
    /// Optional `username:` value from the publisher entry config.
    pub config_username: Option<&'a str>,
    /// Optional `password:` value from the publisher entry config.
    pub config_password: Option<&'a str>,
    /// Env-var prefix (e.g. "ARTIFACTORY", "UPLOAD"). Joined with the
    /// upper-cased entry name and `_USERNAME` / `_SECRET`.
    pub env_prefix: &'a str,
    /// When false (artifactory), an unresolved credential pair bails. When
    /// true (upload), an entirely empty pair is acceptable for anonymous
    /// targets and only the half-set state is refused.
    pub anonymous_ok: bool,
}

/// Resolve `(username, password)` for an HTTP-based publisher entry.
///
/// Cascade per field: rendered config value → `<PREFIX>_<NAME>_USERNAME` /
/// `<PREFIX>_<NAME>_SECRET` env var. Empty-after-render falls through to
/// env so a half-edited YAML does not silently ship anonymous.
///
/// Refuses (with a clear "set X or env Y" message):
/// - Half-set credential pair under any spec.
/// - Empty pair when `anonymous_ok = false` (artifactory).
///
/// Skipped in dry-run so config previews do not require real secrets.
pub(crate) fn resolve_http_credentials(
    ctx: &Context,
    spec: &CredentialResolveSpec<'_>,
) -> Result<(String, String)> {
    let env_map = ctx.template_vars().all_env();
    let lookup_env = |name: &str| -> Option<String> {
        env_map
            .get(name)
            .cloned()
            .or_else(|| ctx.env_var(name))
            .filter(|s| !s.is_empty())
    };

    let name_upper = spec.entry_name.to_uppercase().replace('-', "_");
    let username_env = format!("{}_{}_USERNAME", spec.env_prefix, name_upper);
    let password_env = format!("{}_{}_SECRET", spec.env_prefix, name_upper);

    // Username: render config (if set), fall through on empty render.
    let username = match spec.config_username {
        Some(u) => {
            let rendered = ctx.render_template(u).with_context(|| {
                format!(
                    "{}: failed to render username for '{}'",
                    spec.publisher, spec.entry_name
                )
            })?;
            if rendered.is_empty() {
                lookup_env(&username_env).unwrap_or_default()
            } else {
                rendered
            }
        }
        None => lookup_env(&username_env).unwrap_or_default(),
    };

    // Password: config wins over env so a YAML setting isn't shadowed by
    // a stale shell env var.
    let password = spec
        .config_password
        .and_then(|p| ctx.render_template(p).ok())
        .filter(|p| !p.is_empty())
        .or_else(|| lookup_env(&password_env))
        .unwrap_or_default();

    if !ctx.is_dry_run() {
        match (username.is_empty(), password.is_empty()) {
            (false, true) => bail!(
                "{}: '{}' has username set but no password \
                 (set 'password:' in config or {} in env)",
                spec.publisher,
                spec.entry_name,
                password_env
            ),
            (true, false) => bail!(
                "{}: '{}' has password set but no username \
                 (set 'username:' in config or {} in env)",
                spec.publisher,
                spec.entry_name,
                username_env
            ),
            (true, true) if !spec.anonymous_ok => bail!(
                "{}: '{}' resolved with no credentials \
                 (set username/password in config or {} / {} in env; \
                 anonymous upload is refused)",
                spec.publisher,
                spec.entry_name,
                username_env,
                password_env
            ),
            _ => {}
        }
    }

    Ok((username, password))
}

/// Format the single default-verbosity summary line for one HTTP upload entry,
/// collapsing the per-artifact `uploaded …` / `skipped …` firehose into one
/// line. `uploaded` counts artifacts this run PUT/POSTed (fresh or
/// overwritten); `skipped` counts artifacts already present byte-identical (no
/// request issued). `destination` is the entry name the bytes landed under.
pub(crate) fn upload_summary(uploaded: usize, skipped: usize, destination: &str) -> String {
    format!("uploaded {uploaded} artifact(s), skipped {skipped} (already present) → {destination}")
}

/// Tally of an HTTP upload-entry run, shared by Artifactory + generic uploads.
///
/// `uploaded` counts artifacts PUT/POSTed this run (fresh or overwritten);
/// `already_present` counts those skipped because an identical SHA-256 copy
/// was already at the target path (idempotent re-run).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadEntryCounts {
    pub uploaded: usize,
    pub already_present: usize,
}

/// Per-artifact request parameters shared across an entry's whole upload set.
///
/// Bundles the request-shaping inputs that are constant for every artifact in
/// one upload entry (publisher label, method, checksum header, custom headers,
/// basic-auth, and the `custom_artifact_name` URL-append toggle) so the shared
/// driver can thread them through without a long argument list.
pub(crate) struct UploadEntryRequest<'a> {
    pub publisher: &'a str,
    pub method: &'a str,
    pub checksum_header: &'a str,
    pub custom_headers: &'a std::collections::HashMap<String, String>,
    pub username: &'a str,
    pub password: &'a str,
    pub custom_artifact_name: bool,
    pub overwrite: bool,
}

/// Upload one entry's resolved artifact set over the shared HTTP-PUT/POST
/// machinery, returning the uploaded / already-present tally.
///
/// This is the single per-artifact upload loop both the Artifactory and the
/// generic `uploads:` publisher drive. The per-artifact target URL is rendered
/// through [`render_artifact_url`] (the same template path dry-run previews
/// use); `rewrite_url` lets a caller post-process each rendered URL
/// (Artifactory appends Debian matrix params there) before the bytes move —
/// an identity closure (`|url, _| Ok(url.to_string())`) opts out. Each upload
/// goes through
/// [`crate::artifactory::upload_single_artifact`], which carries the
/// idempotency probe, retry budget, and checksum/custom-header application.
/// One serially-prepared upload: the artifact, its fully rendered target URL,
/// and its rendered custom-header set. Prepared in the serial pre-pass (all
/// three touch the non-`Sync` `ctx`) so the network PUT can run on a worker
/// thread.
type UploadJob<'a> = (&'a Artifact, String, Vec<(String, String)>);

/// `parallelism` bounds how many idempotent PUTs/POSTs run concurrently. Each
/// upload targets a distinct, independent path and carries its own idempotency
/// probe + retry budget, so the only safe-ordering concern (Assets→Manager→
/// Submitter group order) is unaffected — that boundary lives one level up in
/// the dispatch loop, never inside one entry's artifact set. URL rendering
/// stays serial (cheap, deterministic) before the network fan-out so the
/// `rewrite_url` closure never has to cross threads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_artifact_set(
    ctx: &Context,
    client: &reqwest::blocking::Client,
    target_template: &str,
    artifacts: &[Artifact],
    req: &UploadEntryRequest<'_>,
    policy: &RetryPolicy,
    parallelism: usize,
    log: &StageLogger,
    mut rewrite_url: impl FnMut(&str, &Artifact) -> Result<String>,
) -> Result<UploadEntryCounts> {
    // Idempotent uploads keep a transient-error retry floor even when a
    // stateful mode (`--publish-only`) resolves `max_attempts` to 1.
    let policy = policy.with_idempotent_floor();

    // Render every target URL AND custom-header set serially: both touch the
    // non-`Sync` `ctx` (URL rewrite hook = Artifactory's Debian matrix-param
    // append; header templates = per-artifact `ctx.template_vars()`), so they
    // must complete before the network fan-out. The rendered order is
    // deterministic; only the network PUTs run concurrently below.
    let jobs: Vec<UploadJob<'_>> = artifacts
        .iter()
        .map(|artifact| {
            let url =
                render_artifact_url(ctx, target_template, artifact, req.custom_artifact_name)?;
            let url = rewrite_url(&url, artifact)?;
            let rendered_headers =
                crate::artifactory::render_custom_headers(ctx, req.custom_headers, artifact)?;
            Ok((artifact, url, rendered_headers))
        })
        .collect::<Result<_>>()?;

    let outcomes = run_parallel_chunks(
        &jobs,
        parallelism,
        "http upload",
        |(artifact, url, rendered_headers)| {
            crate::artifactory::upload_single_artifact_prepared(
                client,
                &UploadHeaders {
                    publisher: req.publisher,
                    method: req.method,
                    url,
                    checksum_header: req.checksum_header,
                },
                &UploadAuth {
                    username: req.username,
                    password: req.password,
                },
                artifact,
                req.overwrite,
                rendered_headers,
                &policy,
                log,
            )
        },
    )?;

    let mut counts = UploadEntryCounts::default();
    for outcome in outcomes {
        match outcome {
            UploadOutcome::Uploaded => counts.uploaded += 1,
            UploadOutcome::AlreadyPresent => counts.already_present += 1,
        }
    }
    Ok(counts)
}

/// Refuse a half-set mTLS pair. Both crates need the same exact check.
pub(crate) fn validate_mtls_pair(
    publisher: &str,
    entry_name: &str,
    cert: Option<&str>,
    key: Option<&str>,
) -> Result<()> {
    if cert.is_some() != key.is_some() {
        bail!(
            "{}: '{}' has only one of client_x509_cert / client_x509_key set \
             (set both to enable mTLS, or leave both empty)",
            publisher,
            entry_name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use anodizer_core::retry::{IDEMPOTENT_PUT_ATTEMPTS, RetryPolicy};
    use std::time::Duration;

    /// The HTTP-upload set applies the shared idempotent floor to its resolved
    /// policy (`upload_artifact_set` calls `policy.with_idempotent_floor()`).
    /// A `--publish-only`-shaped `attempts: 1` is raised to the floor; an
    /// operator cap above the floor is preserved. Fails if the floor reverts.
    #[test]
    fn idempotent_floor_raises_single_attempt_preserves_higher() {
        let base = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        assert_eq!(
            base.with_idempotent_floor().max_attempts,
            IDEMPOTENT_PUT_ATTEMPTS,
            "a single-attempt upload policy must be floored to the idempotent minimum"
        );
        assert_eq!(
            RetryPolicy {
                max_attempts: 7,
                ..base
            }
            .with_idempotent_floor()
            .max_attempts,
            7,
            "an operator cap above the floor must be preserved, not lowered"
        );
    }
}
