//! GemFury publish orchestration — uploads each matching artifact via
//! `POST https://push.fury.io/<account>` with HTTP Basic auth (the push
//! token as the username, empty password — Fury's documented surface).
//!
//! Token handling:
//!   * Push token resolved from `cfg.token` (templated) or the env var
//!     named by `secret_name` (default `FURY_PUSH_TOKEN`). NEVER logged.
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
use anodizer_core::retry::{
    RetryPolicy, SuccessClass, http_status, retry_http_blocking, retry_sync,
};
use anyhow::{Context as _, Result, bail};

/// Outcome of [`publish_to_gemfury`]: one [`GemFuryTarget`] per artifact
/// actually pushed. The caller drives rollback evidence off this list so
/// `--rollback-only` can issue a real per-version DELETE against the Fury
/// API. Skips (skip / dry-run / `if` falsy / idempotent-already-
/// pushed) produce no target entry — rollback only undoes what THIS run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GemFuryTarget {
    /// GemFury account name.
    pub account: String,
    /// Fury-visible package name (e.g. `mytool`), derived from the artifact
    /// filename via [`fury_package_name`]. The rollback DELETE keys on this,
    /// so it must match the name Fury exposes — NOT the full artifact filename.
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

/// Upper bound on concurrent Fury pushes within one account entry. Each push
/// targets a distinct `<package>@<version>` and is independently idempotent,
/// so they fan out safely; the cap keeps the multipart upload set from
/// saturating the connection pool or tripping Fury's rate limiter.
const MAX_PUSH_CONCURRENCY: usize = 4;

/// Default env var name carrying the push token.
const DEFAULT_PUSH_TOKEN_ENV: &str = "FURY_PUSH_TOKEN";
/// Default env var name carrying the API (delete) token.
const DEFAULT_API_TOKEN_ENV: &str = "FURY_API_TOKEN";
/// Base URL for the push endpoint. The account name is appended per call.
pub(crate) const PUSH_BASE: &str = "https://push.fury.io";
/// Base URL for the API (used by version probe + delete).
pub(crate) const API_BASE: &str = "https://api.fury.io";

/// Resolve the push base URL from an injected [`EnvSource`]. Defaults to
/// [`PUSH_BASE`]; `ANODIZE_GEMFURY_PUSH_BASE` overrides it so tests can point
/// the push at a local responder. Threading the read through an `EnvSource`
/// (rather than `std::env::var`) lets a test inject the base via a
/// [`MapEnvSource`](anodizer_core::MapEnvSource) without mutating the process
/// env — eliminating the cross-test race where one test's base points another
/// test's HTTP probe at a torn-down listener. Production never sets the
/// variable.
pub(crate) fn push_base_from<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> String {
    env.var("ANODIZE_GEMFURY_PUSH_BASE")
        .unwrap_or_else(|| PUSH_BASE.to_string())
}

/// Resolve the API base URL (version probe + delete) from an injected
/// [`EnvSource`]. Defaults to [`API_BASE`]; `ANODIZE_GEMFURY_API_BASE`
/// overrides it for tests. See [`push_base_from`] for why the read is
/// `EnvSource`-routed rather than a bare `std::env::var`.
pub(crate) fn api_base_from<E: anodizer_core::EnvSource + ?Sized>(env: &E) -> String {
    env.var("ANODIZE_GEMFURY_API_BASE")
        .unwrap_or_else(|| API_BASE.to_string())
}

/// Best-effort Fury package name from an artifact filename.
///
/// Fury exposes a package under its control-file name (e.g. `mytool`), NOT
/// the full artifact filename (`mytool_1.2.3_amd64.deb`). Probing with the
/// full filename always 404s. Derive the package name by truncating the
/// filename at the first occurrence of the version string, then trimming a
/// trailing `_`/`-`/`.` separator (deb uses `name_version_arch`, rpm/apk use
/// `name-version`). Falls back to the extension-stripped basename when the
/// version doesn't appear (e.g. a snapshot-renamed archive), which is still
/// a closer key than the raw filename.
pub(crate) fn fury_package_name(art_name: &str, version: &str) -> String {
    if !version.is_empty()
        && let Some(idx) = art_name.find(version)
    {
        let head = &art_name[..idx];
        let trimmed = head.trim_end_matches(['_', '-', '.']);
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // No version match: strip a known package extension and return the rest.
    for ext in [".deb", ".rpm", ".apk"] {
        if let Some(stripped) = art_name.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    art_name.to_string()
}

/// Resolved push-token env var name for the given config entry.
pub(crate) fn push_token_env_var(cfg: &GemFuryConfig) -> &str {
    cfg.secret_name.as_deref().unwrap_or(DEFAULT_PUSH_TOKEN_ENV)
}

/// Resolved API-token env var name for the given config entry.
pub(crate) fn api_token_env_var(cfg: &GemFuryConfig) -> &str {
    cfg.api_secret_name
        .as_deref()
        .unwrap_or(DEFAULT_API_TOKEN_ENV)
}

/// Detect the Fury format from a filename extension. Returns `None` for
/// unrecognized extensions so the caller can skip non-matching artifacts.
///
/// Case-insensitive on the extension so it agrees with the case-folding
/// artifact filter (`util::format_matches`): an uppercase-extension artifact
/// (e.g. `myapp.DEB`) that PASSES the filter must also be detected here,
/// otherwise the publish path would hit the "filter should have excluded it"
/// error on an artifact the filter deliberately admitted.
pub(crate) fn detect_gemfury_format(filename: &str) -> Option<&'static str> {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".deb") {
        Some("deb")
    } else if lower.ends_with(".rpm") {
        Some("rpm")
    } else if lower.ends_with(".apk") {
        Some("apk")
    } else {
        None
    }
}

/// Default `gemfury[].formats` value.
pub(crate) fn default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Resolve the push token for the given entry: `cfg.token` (templated)
/// wins; otherwise the env var named by `secret_name`. Empty string when
/// both unset — caller surfaces a clear "missing token" error rather than
/// invoking the push anonymously.
pub(crate) fn resolve_push_token(ctx: &Context, cfg: &GemFuryConfig) -> Result<String> {
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
pub(crate) fn resolve_api_token(ctx: &Context, cfg: &GemFuryConfig) -> Result<String> {
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
pub(crate) fn resolve_formats(cfg: &GemFuryConfig) -> Vec<String> {
    match cfg.formats.as_ref() {
        Some(v) if !v.is_empty() => v.clone(),
        _ => default_formats().into_iter().map(String::from).collect(),
    }
}

/// Probe Fury for an already-published `<package>@<version>`.
///
/// Returns `Ok(true)` when the version is present, `Ok(false)` only on a
/// definitive `404` (the version genuinely does not exist on Fury).
///
/// Fail-closed on an inconclusive probe: a transport/connect failure or any
/// non-404 HTTP shape (5xx, auth failure, rate-limit) surfaces an `Err`
/// rather than `Ok(false)`. A Fury push can be irreversible for up to 72h
/// after upload, so a probe that *cannot prove* the version is absent must
/// not green-light the push — assuming "not published" on an outage would
/// re-push over an existing version the moment the registry recovers. The
/// caller aborts this artifact's push and records the failure for the
/// operator instead.
///
/// Endpoint: `GET https://api.fury.io/<account>/packages/<name>/versions/<version>`.
/// HTTP Basic auth (push token as username).
#[allow(clippy::too_many_arguments)]
pub(crate) fn version_already_published<E: anodizer_core::EnvSource + ?Sized>(
    client: &reqwest::blocking::Client,
    account: &str,
    package: &str,
    version: &str,
    push_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    env: &E,
) -> Result<bool> {
    let url = format!(
        "{}/{}/packages/{}/versions/{}",
        api_base_from(env),
        account,
        package,
        version
    );
    log.verbose(&format!("probing GET {}", url));
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
            // 404 is the documented "not present" response — the only shape
            // that proves the version is absent, surfaced as `false` so the
            // publish proceeds. Any other shape (5xx, auth, rate-limit) or a
            // transport failure leaves presence UNKNOWN; bail rather than
            // publish blind to a registry that is irreversible for up to 72h.
            if http_status(&err) == 404 {
                return Ok(false);
            }
            log.warn(&format!(
                "gemfury idempotency probe for '{}@{}' was inconclusive (not a 404): {}; \
                 refusing to publish blind to a registry that is irreversible for up to 72h — \
                 retry once Fury is healthy",
                package,
                version,
                redact_bearer_tokens(&format!("{err:#}"))
            ));
            Err(err.context(format!(
                "gemfury: idempotency probe for '{}@{}' returned an inconclusive non-404 error",
                package, version
            )))
        }
    }
}

/// One pre-validated artifact slated for a Fury push. The borrow of `path`
/// keeps the (large) artifact bytes on disk until the worker reads them, so
/// only the in-flight chunk holds file contents in memory.
struct PushJob<'a> {
    path: &'a std::path::Path,
    art_name: String,
    format: String,
}

/// Terminal state of one artifact's push, folded back into the rollback
/// `pushed` list in submission order by the caller.
enum PushOutcome {
    /// Bytes landed this run — becomes a rollback target.
    Pushed(GemFuryTarget),
    /// Already on Fury (idempotency probe hit or 409/422 conflict-as-success);
    /// NOT a rollback target — this run did not place it.
    AlreadyPresent,
}

/// Probe-then-push one artifact to Fury, returning whether it landed (a
/// rollback target) or was an idempotent no-op. Self-contained so the account
/// loop can run it under bounded parallelism: it builds its own multipart
/// body, carries its own retry budget (floored to
/// [`RetryPolicy::with_idempotent_floor`]), and
/// touches no shared mutable state.
#[allow(clippy::too_many_arguments)]
fn push_one_artifact<E: anodizer_core::EnvSource + ?Sized>(
    client: &reqwest::blocking::Client,
    account: &str,
    version: &str,
    push_url: &str,
    push_token: &str,
    cfg: &GemFuryConfig,
    job: &PushJob<'_>,
    policy: &RetryPolicy,
    log: &StageLogger,
    env: &E,
) -> Result<PushOutcome> {
    let art_name = &job.art_name;

    // Idempotency probe: skip if `<package>@<version>` is already on Fury —
    // matches the immutable-releases policy (re-run on an already-pushed tag
    // must not error). Fury exposes a package in /packages/<name>/ under its
    // control-file name (the artifact filename minus the version+arch+
    // extension suffix), so the probe keys on the derived name — probing the
    // raw filename always 404s. A 404 here just means we'll attempt the push
    // (which has its own 409/422 conflict-as-success guard for the racing
    // case).
    let fury_pkg = fury_package_name(art_name, version);
    if version_already_published(
        client, account, &fury_pkg, version, push_token, policy, log, env,
    )? {
        log.status(&format!(
            "skipped '{}@{}' — already on gemfury account '{}' (idempotent)",
            fury_pkg, version, account
        ));
        return Ok(PushOutcome::AlreadyPresent);
    }

    log.status(&format!(
        "pushing {} ({}) → {} (gemfury account '{}')",
        art_name, job.format, push_url, account
    ));

    let file_bytes = std::fs::read(job.path)
        .with_context(|| format!("gemfury: read '{}'", job.path.display()))?;

    // Idempotent pushes keep a transient-error retry floor even when a
    // stateful mode (`--publish-only`) resolves `max_attempts` to 1.
    let push_policy = policy.with_idempotent_floor();
    let max_attempts = push_policy.max_attempts;
    let mime = "application/octet-stream";
    // Set inside the retry closure when the push returns a 409/422
    // already-exists conflict, so the post-retry code can skip recording a
    // rollback target. `Cell` because the closure is `FnMut`.
    let conflict_skipped = std::cell::Cell::new(false);
    retry_sync(&push_policy, |attempt| {
        if attempt > 1 {
            log.warn(&format!(
                "gemfury push attempt {}/{} failed (transient), retrying…",
                attempt - 1,
                max_attempts
            ));
        }
        let file_part = match reqwest::blocking::multipart::Part::bytes(file_bytes.clone())
            .file_name(art_name.clone())
            .mime_str(mime)
        {
            Ok(p) => p,
            Err(e) => {
                return Err(ControlFlow::Break(anyhow::Error::new(e).context(format!(
                    "gemfury: build multipart part for '{}' (mime '{}')",
                    art_name, mime
                ))));
            }
        };
        let form = reqwest::blocking::multipart::Form::new().part("package", file_part);
        let req = client
            .post(push_url)
            .basic_auth(push_token, Some(""))
            .multipart(form);
        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => {
                // Transport-level failure — retry.
                return Err(ControlFlow::Continue(
                    anyhow::Error::new(e).context(format!("gemfury: send POST {}", push_url)),
                ));
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // Idempotent conflict: a 409 (Conflict) / 422 (Unprocessable) means the
        // version already exists on Fury — a re-run on an already-published
        // tag, or a racing concurrent uploader. The operator's intent ("land
        // this artifact") is satisfied, so treat it as success rather than a
        // hard failure (mirrors the cloudsmith conflict-as-success guard).
        if matches!(status.as_u16(), 409 | 422) {
            conflict_skipped.set(true);
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

    // A conflict-as-success push means the version was already present (re-run
    // / racing uploader); record NO rollback target — this run did not place
    // it, so rollback must not delete it.
    if conflict_skipped.get() {
        log.status(&format!(
            "'{}@{}' already on gemfury account '{}' (push conflict) — treated as idempotent",
            fury_pkg, version, account
        ));
        return Ok(PushOutcome::AlreadyPresent);
    }

    Ok(PushOutcome::Pushed(GemFuryTarget {
        account: account.to_string(),
        // Record the Fury-visible package name (not the artifact filename) so
        // rollback's DELETE /packages/<name>/versions/… keys on the same name
        // the probe / skip-log / conflict-log use — a full-filename key 404s
        // and orphans the artifact.
        package: fury_pkg,
        version: version.to_string(),
        format: job.format.clone(),
        push_token_env_var: push_token_env_var(cfg).to_string(),
        api_token_env_var: api_token_env_var(cfg).to_string(),
    }))
}

/// Top-level publish entrypoint. Iterates each `gemfury[]` entry and pushes
/// every matching artifact via `POST push.fury.io/<account>` with HTTP Basic
/// auth, appending one [`GemFuryTarget`] to `pushed` per artifact that
/// actually landed.
///
/// `pushed` is an out-param (rather than the return value) so that on a
/// mid-loop error the caller still holds the partial set of artifacts that
/// DID land before the failure — those must be rolled back, not orphaned.
/// A `?` on the previous `Result<Vec<_>>` signature discarded that evidence.
pub fn publish_to_gemfury(
    ctx: &Context,
    log: &StageLogger,
    pushed: &mut Vec<GemFuryTarget>,
) -> Result<()> {
    let entries = match ctx.config.gemfury {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    let policy = ctx.retry_policy();

    for (idx, cfg) in entries.iter().enumerate() {
        let label = cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("gemfury[{}]", idx));
        log.status(&format!("processing gemfury package '{}'", label));

        // ---- Skip gate ----
        if let Some(skip) = cfg.skip.as_ref() {
            let off = skip
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .context("gemfury: render skip template")?;
            if off {
                log.status("skipped gemfury entry — skip evaluates true");
                continue;
            }
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("gemfury entry '{}'", label),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped gemfury entry — `if` condition evaluated falsy");
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

        let id_format_filtered: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| matches!(a.kind, ArtifactKind::LinuxPackage | ArtifactKind::Archive))
            .filter(|a| crate::util::matches_id_filter(a, cfg.ids.as_deref()))
            // Keep only artifacts whose extension is in the configured
            // formats filter. The shared case-folding matcher subsumes the
            // per-extension hand-roll (gemfury slugs deb/rpm/apk equal the
            // file extensions); `detect_gemfury_format` is still used below
            // to record the slug on the published target.
            .filter(|a| crate::util::format_matches(a.name(), &formats))
            .collect();
        // `exclude:` glob filter applied last so the pre-exclude count drives
        // the eliminated-all warning (a typo'd glob silently dropping every
        // package).
        let pre_exclude = id_format_filtered.len();
        let artifacts: Vec<_> = id_format_filtered
            .into_iter()
            .filter(|a| anodizer_core::artifact::passes_exclude_filter(a, cfg.exclude.as_deref()))
            .collect();
        if anodizer_core::artifact::exclude_filter_eliminated_all(
            cfg.exclude.as_deref(),
            pre_exclude,
            artifacts.len(),
        ) {
            log.warn(&format!(
                "exclude filter {:?} dropped all {} candidate package(s) for GemFury \
                 account '{}'; check the globs match asset names, not full paths",
                cfg.exclude.as_deref().unwrap_or_default(),
                pre_exclude,
                account
            ));
        }

        // ---- Dry-run logging ----
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would push {} artifact(s) to GemFury account '{}'",
                artifacts.len(),
                account
            ));
            for a in &artifacts {
                log.status(&format!("(dry-run) {} ({})", a.name(), a.kind));
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
                "no matching gemfury artifacts for account '{}' (formats: {:?})",
                account, formats
            ));
            continue;
        }

        let client = anodizer_core::http::blocking_client(Duration::from_secs(60))
            .context("gemfury: build HTTP client")?;

        let version = ctx.version();
        let push_url = format!("{}/{}", push_base_from(ctx.env_source()), account);

        // Pre-validate every artifact's existence + format serially so a
        // missing file or unrecognized extension fails fast before any push
        // fans out (and before partial pushes can land).
        let mut jobs: Vec<PushJob<'_>> = Vec::with_capacity(artifacts.len());
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
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "gemfury: artifact '{}' has no recognized package format \
                         (expected .deb/.rpm/.apk); the artifact filter should have \
                         excluded it",
                        art_name
                    )
                })?
                .to_string();
            jobs.push(PushJob {
                path,
                art_name,
                format,
            });
        }

        // Idempotent pushes to distinct `<package>@<version>` paths fan out
        // bounded by the global parallelism (capped at MAX_PUSH_CONCURRENCY).
        // The Assets→Manager→Submitter group ordering is untouched: gemfury is
        // one Assets-group publisher, and only its own independent artifact
        // pushes run concurrently.
        //
        // Unlike the order-preserving-but-fail-fast `run_parallel_chunks`, the
        // rollback contract here demands EVERY artifact that actually POSTed be
        // recorded — even a sibling that succeeded concurrently with a failing
        // push must land in `pushed` so the partial set can be rolled back. So
        // each chunk runs to completion, ALL `Pushed` outcomes are recorded in
        // submission order, and only then is the first error surfaced.
        let parallelism = ctx.options.parallelism.clamp(1, MAX_PUSH_CONCURRENCY);
        // Surface the clamp under -v when it actually lowers the operator's
        // global parallelism, so a higher --parallelism that silently has no
        // effect on Fury pushes is discoverable rather than mysterious.
        if ctx.options.parallelism > MAX_PUSH_CONCURRENCY {
            log.verbose(&format!(
                "gemfury push concurrency clamped to {MAX_PUSH_CONCURRENCY} \
                 (global parallelism {} is higher; Fury rate limits favor a lower cap)",
                ctx.options.parallelism
            ));
        }
        // Resolve the env source to a `Sync` `Arc` ahead of the fan-out so the
        // worker closure captures it (not the non-`Sync` `ctx`).
        let env = ctx.env_source_arc();
        let mut first_err: Option<anyhow::Error> = None;
        'chunks: for chunk in jobs.chunks(parallelism) {
            let chunk_results: Vec<Result<PushOutcome>> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|job| {
                        scope.spawn(|| {
                            push_one_artifact(
                                &client,
                                &account,
                                &version,
                                &push_url,
                                &push_token,
                                cfg,
                                job,
                                &policy,
                                log,
                                env.as_ref(),
                            )
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        anodizer_core::parallel::join_panic_to_err(h.join(), "gemfury push")
                            .and_then(|r| r)
                    })
                    .collect()
            });
            // Record landed targets in submission order before propagating any
            // error, so a concurrent success is never dropped from rollback.
            for result in chunk_results {
                match result {
                    Ok(PushOutcome::Pushed(target)) => pushed.push(target),
                    Ok(PushOutcome::AlreadyPresent) => {}
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            // A failure stops launching further chunks (the release is already
            // doomed); the in-flight chunk's successes are kept above.
            if first_err.is_some() {
                break 'chunks;
            }
        }

        if let Some(err) = first_err {
            return Err(err);
        }

        log.status(&format!(
            "gemfury push complete for account '{}' ({} artifact(s))",
            account,
            artifacts.len()
        ));
    }

    Ok(())
}

/// Issue `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`
/// against the Fury API. Used by [`crate::gemfury::publisher::GemFuryPublisher::rollback`].
/// Returns Ok on 2xx; bubbles a 4xx/5xx error chain with a redacted body.
#[allow(clippy::too_many_arguments)]
pub fn delete_version<E: anodizer_core::EnvSource + ?Sized>(
    client: &reqwest::blocking::Client,
    account: &str,
    package: &str,
    version: &str,
    api_token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    env: &E,
) -> Result<()> {
    let url = format!(
        "{}/{}/packages/{}/versions/{}",
        api_base_from(env),
        account,
        package,
        version
    );
    log.verbose(&format!("DELETE {}", url));
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
