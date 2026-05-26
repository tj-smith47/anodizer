//! GemFury publish orchestration — uploads each matching artifact via
//! `POST https://push.fury.io/<account>` with HTTP Basic auth (the push
//! token as the username, empty password — Fury's documented surface).
//!
//! Token handling:
//!   * Push token resolved from `cfg.token` (templated) or the env var
//!     named by `secret_name` (default `FURY_TOKEN`). NEVER logged.
//!   * API (delete) token resolved from `cfg.api_token` or the env var
//!     named by `api_secret_name` (default `FURY_API_TOKEN`). Used only
//!     by rollback.

use std::ops::ControlFlow;
use std::time::Duration;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{ArchivesConfig, GemFuryConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{HttpError, RetryPolicy, SuccessClass, retry_http_blocking, retry_sync};
use anyhow::{Context as _, Result, bail};

/// Outcome of [`publish_to_gemfury`]: one [`GemFuryTarget`] per artifact
/// actually pushed. The caller drives rollback evidence off this list so
/// `--rollback-only` can issue a real per-version DELETE against the Fury
/// API. Skips (skip / disable / dry-run / `if` falsy / idempotent-already-
/// pushed) produce no target entry — rollback only undoes what THIS run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GemFuryTarget {
    /// GemFury account name.
    pub account: String,
    /// Package basename pushed (e.g. `mytool_1.2.3_amd64.deb`).
    pub package: String,
    /// Published version (semver string).
    pub version: String,
    /// Format detected from the filename extension (`deb` / `rpm` / `apk`).
    pub format: String,
    /// Env var NAME the rollback path consults to re-resolve the push token.
    pub push_token_env_var: String,
    /// Env var NAME the rollback path consults to re-resolve the API token.
    pub api_token_env_var: String,
}

/// Default env var name carrying the push token.
const DEFAULT_PUSH_TOKEN_ENV: &str = "FURY_TOKEN";
/// Default env var name carrying the API (delete) token.
const DEFAULT_API_TOKEN_ENV: &str = "FURY_API_TOKEN";
/// Base URL for the push endpoint. The account name is appended per call.
pub(crate) const PUSH_BASE: &str = "https://push.fury.io";
/// Base URL for the API (used by version probe + delete).
pub(crate) const API_BASE: &str = "https://api.fury.io";

/// Resolved push-token env var name for the given config entry.
pub fn push_token_env_var(cfg: &GemFuryConfig) -> &str {
    cfg.secret_name.as_deref().unwrap_or(DEFAULT_PUSH_TOKEN_ENV)
}

/// Resolved API-token env var name for the given config entry.
pub fn api_token_env_var(cfg: &GemFuryConfig) -> &str {
    cfg.api_secret_name
        .as_deref()
        .unwrap_or(DEFAULT_API_TOKEN_ENV)
}

/// Detect the Fury format from a filename extension. Returns `None` for
/// unrecognized extensions so the caller can skip non-matching artifacts.
pub fn detect_gemfury_format(filename: &str) -> Option<&'static str> {
    if filename.ends_with(".deb") {
        Some("deb")
    } else if filename.ends_with(".rpm") {
        Some("rpm")
    } else if filename.ends_with(".apk") {
        Some("apk")
    } else {
        None
    }
}

/// Default formats matching GoReleaser Pro's `gemfury[].formats` default.
pub fn default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Resolve the push token for the given entry: `cfg.token` (templated)
/// wins; otherwise the env var named by `secret_name`. Empty string when
/// both unset — caller surfaces a clear "missing token" error rather than
/// invoking the push anonymously.
pub fn resolve_push_token(ctx: &Context, cfg: &GemFuryConfig) -> Result<String> {
    if let Some(raw) = cfg.token.as_deref()
        && !raw.is_empty()
    {
        let rendered = ctx
            .render_template(raw)
            .context("gemfury: render push-token template")?;
        if !rendered.is_empty() {
            return Ok(rendered);
        }
    }
    let env = ctx.env_source();
    Ok(env
        .var(push_token_env_var(cfg))
        .unwrap_or_default()
        .to_string())
}

/// Resolve the API (delete) token. Same shape as [`resolve_push_token`]
/// but consults `cfg.api_token` / `api_secret_name`. The rollback path
/// is the only consumer; an empty result causes rollback to fall through
/// to the warn-only manual-cleanup checklist.
pub fn resolve_api_token(ctx: &Context, cfg: &GemFuryConfig) -> Result<String> {
    if let Some(raw) = cfg.api_token.as_deref()
        && !raw.is_empty()
    {
        let rendered = ctx
            .render_template(raw)
            .context("gemfury: render api-token template")?;
        if !rendered.is_empty() {
            return Ok(rendered);
        }
    }
    let env = ctx.env_source();
    Ok(env
        .var(api_token_env_var(cfg))
        .unwrap_or_default()
        .to_string())
}

/// Walk crate-level `archives:` blocks and bail when one declares multiple
/// formats AND the artifact set for that crate contains more than one
/// format extension matching the gemfury formats filter. Without this the
/// publisher would silently push every variant which is rarely what the
/// operator wanted.
fn preflight_multi_format_unambiguous(ctx: &Context, cfg: &GemFuryConfig) -> Result<()> {
    let id_filter = cfg.ids.as_ref();
    for krate in &ctx.config.crates {
        let matches = match id_filter {
            Some(ids) => ids.iter().any(|id| id == &krate.name),
            None => true,
        };
        if !matches {
            continue;
        }
        let configs = match &krate.archives {
            ArchivesConfig::Configs(c) => c,
            ArchivesConfig::Disabled => continue,
        };
        for archive in configs {
            let Some(formats) = archive.formats.as_ref() else {
                continue;
            };
            // Only fail when MORE than one of the multi-format archive
            // variants would actually land in gemfury (i.e. is in the
            // configured formats filter). Two-format archives where only
            // one extension is in the gemfury filter (e.g. `tar.gz` + `deb`)
            // do NOT trip — the publisher pushes only the `deb`.
            let configured_formats = resolve_formats(cfg);
            let overlap: Vec<&String> = formats
                .iter()
                .filter(|f| configured_formats.iter().any(|cf| cf == f.as_str()))
                .collect();
            if overlap.len() > 1 {
                bail!(
                    "gemfury publisher for crate {}: archive declares multiple package formats {:?} \
                     which overlap with the configured gemfury formats filter — narrow `formats:` \
                     on the gemfury entry so exactly one extension is pushed",
                    krate.name,
                    overlap
                );
            }
        }
    }
    Ok(())
}

/// Return the configured formats filter (or the default
/// `["apk","deb","rpm"]`).
pub fn resolve_formats(cfg: &GemFuryConfig) -> Vec<String> {
    match cfg.formats.as_ref() {
        Some(v) if !v.is_empty() => v.clone(),
        _ => default_formats().into_iter().map(String::from).collect(),
    }
}

/// Probe Fury for an already-published `<package>@<version>`. Returns
/// `Ok(true)` when the version is present, `Ok(false)` when 404 or any
/// other transient/spawn failure (so the publish path still runs and
/// surfaces the real error).
///
/// Endpoint: `GET https://api.fury.io/<account>/packages/<name>/versions/<version>`.
/// HTTP Basic auth (push token as username).
pub fn version_already_published(
    client: &reqwest::blocking::Client,
    account: &str,
    package: &str,
    version: &str,
    push_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let url = format!(
        "{}/{}/packages/{}/versions/{}",
        API_BASE, account, package, version
    );
    log.verbose(&format!("gemfury: probe GET {}", url));
    let scope = format!("gemfury probe for {}@{}", package, version);
    let result = retry_http_blocking(
        &scope,
        policy,
        SuccessClass::AllowRedirects,
        |_| client.get(&url).basic_auth(push_token, Some("")).send(),
        |status, body| {
            format!(
                "gemfury probe for '{}@{}' returned HTTP {}: {}",
                package,
                version,
                status,
                redact_bearer_tokens(body.trim())
            )
        },
    );
    match result {
        Ok((status, _body)) => Ok(status.is_success() || status.is_redirection()),
        Err(err) => {
            // Walk the anyhow chain looking for the wrapped HTTP status.
            // 404 is the documented "not present" response — surface as
            // `false` so the publish path proceeds; any other shape is
            // unknown and we still default to `false` so the publish runs
            // (the actual upload will surface the real failure).
            let status_in_chain: Option<u16> = err
                .chain()
                .find_map(|e| e.downcast_ref::<HttpError>().map(|h| h.status));
            if matches!(status_in_chain, Some(404)) {
                return Ok(false);
            }
            log.verbose(&format!(
                "gemfury: probe inconclusive for '{}@{}' ({}); proceeding with push",
                package, version, err
            ));
            Ok(false)
        }
    }
}

/// Top-level publish entrypoint. Iterates each `gemfury[]` entry and
/// pushes every matching artifact via `POST push.fury.io/<account>` with
/// HTTP Basic auth.
pub fn publish_to_gemfury(ctx: &Context, log: &StageLogger) -> Result<Vec<GemFuryTarget>> {
    let mut pushed: Vec<GemFuryTarget> = Vec::new();
    let entries = match ctx.config.gemfury {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(pushed),
    };

    let policy = ctx.retry_policy();

    for (idx, cfg) in entries.iter().enumerate() {
        let label = cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("gemfury[{}]", idx));
        log.status(&format!("gemfury: processing '{}'", label));

        // ---- Skip gates ----
        if let Some(skip) = cfg.skip.as_ref() {
            let off = skip
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("gemfury: render skip template")?;
            if off {
                log.status("gemfury: entry skipped — skip evaluates true");
                continue;
            }
        }
        if let Some(disable) = cfg.disable.as_ref() {
            let off = disable
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("gemfury: render disable template")?;
            if off {
                log.status("gemfury: entry skipped — disable evaluates true");
                continue;
            }
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("gemfury entry '{}'", label),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("gemfury: entry skipped — `if` condition evaluated falsy");
            continue;
        }

        // ---- Required pre-flight ----
        let account_raw = match cfg.account.as_deref() {
            Some(a) if !a.trim().is_empty() => a,
            _ => bail!(
                "gemfury: 'account' is required but not set on entry '{}'",
                label
            ),
        };
        let account = ctx
            .render_template(account_raw)
            .with_context(|| format!("gemfury: render account '{}'", account_raw))?;

        preflight_multi_format_unambiguous(ctx, cfg)?;

        let formats = resolve_formats(cfg);

        let artifacts: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| matches!(a.kind, ArtifactKind::LinuxPackage | ArtifactKind::Archive))
            .filter(|a| crate::util::matches_id_filter(a, cfg.ids.as_deref()))
            .filter(|a| {
                detect_gemfury_format(a.name())
                    .is_some_and(|fmt| formats.iter().any(|f| f.eq_ignore_ascii_case(fmt)))
            })
            .collect();

        // ---- Dry-run logging ----
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would push {} artifact(s) to GemFury account '{}'",
                artifacts.len(),
                account
            ));
            for a in &artifacts {
                log.status(&format!("(dry-run)   {} ({})", a.name(), a.kind));
            }
            continue;
        }

        // ---- Token ----
        let push_token = resolve_push_token(ctx, cfg)?;
        if push_token.is_empty() {
            bail!(
                "gemfury: push token is required to push to account '{}' (entry '{}'). \
                 Set `${}` or `gemfury[].token`.",
                account,
                label,
                push_token_env_var(cfg)
            );
        }

        if artifacts.is_empty() {
            log.status(&format!(
                "gemfury: no matching artifacts for account '{}' (formats: {:?})",
                account, formats
            ));
            continue;
        }

        let client = anodizer_core::http::blocking_client(Duration::from_secs(60))
            .context("gemfury: build HTTP client")?;

        let version = ctx.version();
        let push_url = format!("{}/{}", PUSH_BASE, account);

        for artifact in &artifacts {
            let path = &artifact.path;
            if !path.exists() {
                bail!(
                    "gemfury: artifact file not found: {} (account '{}')",
                    path.display(),
                    account
                );
            }
            let art_name = artifact.name().to_string();
            let format = detect_gemfury_format(&art_name)
                .expect("artifact list pre-filtered on detect_gemfury_format")
                .to_string();

            // Idempotency probe: skip if `<package>@<version>` is already on
            // Fury — matches the immutable-releases policy (re-run on an
            // already-pushed tag must not error). The package name Fury
            // exposes in /packages/<name>/ is typically the artifact name
            // minus the version suffix; we use the artifact basename as a
            // best-effort identifier (Fury accepts both formats in the
            // probe URL — a 404 just means we'll attempt the push).
            if version_already_published(
                &client,
                &account,
                &art_name,
                &version,
                &push_token,
                &policy,
                log,
            )? {
                log.status(&format!(
                    "gemfury: '{}@{}' already on account '{}' — skipping (idempotent)",
                    art_name, version, account
                ));
                continue;
            }

            log.status(&format!(
                "gemfury: pushing {} ({}) -> {} (account '{}')",
                art_name, format, push_url, account
            ));

            let file_bytes = std::fs::read(path)
                .with_context(|| format!("gemfury: read '{}'", path.display()))?;

            let max_attempts = policy.max_attempts.max(1);
            let mime = "application/octet-stream";
            retry_sync(&policy, |attempt| {
                if attempt > 1 {
                    log.warn(&format!(
                        "gemfury: push attempt {}/{} failed (transient), retrying…",
                        attempt - 1,
                        max_attempts
                    ));
                }
                let file_part = match reqwest::blocking::multipart::Part::bytes(file_bytes.clone())
                    .file_name(art_name.clone())
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
                };
                let form = reqwest::blocking::multipart::Form::new().part("package", file_part);
                let req = client
                    .post(&push_url)
                    .basic_auth(&push_token, Some(""))
                    .multipart(form);
                let resp = match req.send() {
                    Ok(r) => r,
                    Err(e) => {
                        // Transport-level failure — retry.
                        return Err(ControlFlow::Continue(
                            anyhow::Error::new(e)
                                .context(format!("gemfury: send POST {}", push_url)),
                        ));
                    }
                };
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }
                let body = resp.text().unwrap_or_default();
                let err_msg = format!(
                    "gemfury: POST {} for '{}' returned HTTP {}: {}",
                    push_url,
                    art_name,
                    status,
                    redact_bearer_tokens(body.trim())
                );
                let err = anyhow::anyhow!(err_msg);
                if status.is_server_error() || status.as_u16() == 429 {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            })?;

            pushed.push(GemFuryTarget {
                account: account.clone(),
                package: art_name,
                version: version.clone(),
                format,
                push_token_env_var: push_token_env_var(cfg).to_string(),
                api_token_env_var: api_token_env_var(cfg).to_string(),
            });
        }

        log.status(&format!(
            "gemfury: push complete for account '{}' ({} artifact(s))",
            account,
            artifacts.len()
        ));
    }

    Ok(pushed)
}

/// Issue `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`
/// against the Fury API. Used by [`crate::gemfury::publisher::GemFuryPublisher::rollback`].
/// Returns Ok on 2xx; bubbles a 4xx/5xx error chain with a redacted body.
pub fn delete_version(
    client: &reqwest::blocking::Client,
    account: &str,
    package: &str,
    version: &str,
    api_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<()> {
    let url = format!(
        "{}/{}/packages/{}/versions/{}",
        API_BASE, account, package, version
    );
    log.status(&format!("gemfury: DELETE {}", url));
    let scope = format!("gemfury delete for {}@{}", package, version);
    retry_http_blocking(
        &scope,
        policy,
        SuccessClass::AllowRedirects,
        |_| client.delete(&url).basic_auth(api_token, Some("")).send(),
        |status, body| {
            format!(
                "gemfury delete for '{}@{}' returned HTTP {}: {}",
                package,
                version,
                status,
                redact_bearer_tokens(body.trim())
            )
        },
    )?;
    Ok(())
}
