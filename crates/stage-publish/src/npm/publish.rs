//! NPM publish orchestration — assembles the package tarball(s) and invokes
//! `npm publish` with a per-run `.npmrc` that carries the auth token.
//!
//! Two modes (see [`anodizer_core::config::NpmMode`]):
//!   * `optional-deps` (default): packs + publishes each per-platform package
//!     then the metapackage (so the metapackage's `optionalDependencies`
//!     resolve). The biome / git-cliff pattern — npm's native resolution
//!     selects the matching prebuilt package, no postinstall.
//!   * `postinstall`: packs + publishes a single package whose `postinstall.js`
//!     downloads the matching archive at install time.
//!
//! Auth handling — two mutually exclusive credentials, never anonymous:
//!   * **Token** (`cfg.token` templated, else the `NPM_TOKEN` env var): npm
//!     reads `_authToken` from a process-private `.npmrc` in a `TempDir`; the
//!     token never touches the `npm publish` argv.
//!   * **Trusted Publishing (OIDC)**: under GitHub Actions with `id-token:
//!     write`, npm CLI ≥ 11.5.1 / Node ≥ 22.14.0 exchanges the OIDC token for a
//!     short-lived publish credential when a trusted publisher is configured on
//!     the registry. anodizer writes a token-less `.npmrc` (registry + access)
//!     and threads the `ACTIONS_ID_TOKEN_REQUEST_*` env into the `npm publish`
//!     subprocess so the CLI performs the exchange itself.
//!   * Neither present → hard error (never publish anonymously, never skip).
//!
//! The credential is chosen **per published package** under
//! [`anodizer_core::config::NpmAuthMode`] (`cfg.auth`):
//!   * `auto` (default): the registry is probed for each package's existence
//!     ([`probe_package_existence`]). An EXISTING package prefers OIDC when an
//!     OIDC context is present (else the token); a BRAND-NEW package always uses
//!     the token (Trusted Publishing cannot create a non-existent package) and
//!     errors specifically if only OIDC is available. In `optional-deps` mode
//!     this lets a metapackage with a configured Trusted Publisher use OIDC
//!     while its brand-new per-platform sub-packages use the token, in one run.
//!     When OIDC is chosen for an existing package and the publish FAILS, `auto`
//!     retries with the token (if available) and warns loudly that Trusted
//!     Publishing was not exercised — the release succeeds via the token but the
//!     operator sees the TP gap ([`publish_with_oidc_fallback`]).
//!   * `token`: always the token (errors if none) — the historical behaviour.
//!   * `oidc`: always OIDC (errors if no OIDC context) — strict Trusted
//!     Publishing, NO token fallback (a failed exchange fails the release loud).

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::config::{ArchivesConfig, NpmAuthMode, NpmConfig, NpmMode};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, RetryPolicy, retry_sync_deadline};
use anyhow::{Context as _, Result, bail};
use tempfile::TempDir;

use super::manifest::{
    effective_provenance_override, resolve_access, resolve_name, resolve_registry, resolve_tag,
    token_env_var,
};
use super::optional_deps::generate_layout;
use super::staging::write_deterministic;

pub(crate) use super::auth::*;
pub use super::staging::{
    StagedTarball, assemble_optional_deps_tarball, assemble_postinstall_tarball,
};

/// Outcome of [`publish_to_npm`] for one published package: the coordinates
/// recorded in evidence so a later `--rollback-only --from-run` can attempt
/// `npm unpublish`. `None` is returned for every skip path (skip /
/// dry-run / no-binaries / `if:` falsy) so rollback never targets a package
/// the run did not push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmTarget {
    /// Package name as published (e.g. `@scope/foo`).
    pub package: String,
    /// Published version (semver string).
    pub version: String,
    /// Registry endpoint (e.g. `https://registry.npmjs.org`).
    pub registry: String,
    /// Dist-tag the version was pushed under.
    pub dist_tag: String,
    /// Env var NAME the rollback path consults to re-resolve the token.
    pub token_env_var: String,
}

/// Hard-error when an `archives:` block declares multiple `formats:` AND the
/// postinstall publisher's own `format:` is unset — the postinstall script
/// cannot pick which archive to download. Only relevant in postinstall mode.
fn preflight_multi_format_unambiguous(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
) -> Result<()> {
    if cfg
        .format
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
    {
        return Ok(());
    }
    let id_filter = cfg.ids.as_ref();
    for krate in ctx.config.crate_universe() {
        let matches = if let Some(ids) = id_filter {
            ids.iter().any(|id| id == &krate.name)
        } else {
            krate.name == crate_name
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
            if formats.len() > 1 {
                bail!(
                    "npm publisher for crate {}: archive has multiple formats {:?} \
                     and npm publisher's `format:` is unset — set format: tgz \
                     (or zip) explicitly",
                    krate.name,
                    formats
                );
            }
        }
    }
    Ok(())
}

/// Classify an `npm publish` stderr blob as transient (worth retrying) vs.
/// terminal: HTTP 5xx, ECONNRESET / ETIMEDOUT / EAI_AGAIN socket failures.
fn is_transient_npm_publish_stderr(stderr: &str) -> bool {
    let s = stderr.to_ascii_uppercase();
    s.contains("5XX")
        || s.contains("503")
        || s.contains("502")
        || s.contains("504")
        || s.contains("ECONNRESET")
        || s.contains("ETIMEDOUT")
        || s.contains("EAI_AGAIN")
}

/// Write a per-run `.npmrc` under `cfg_dir` (0600). For [`NpmAuth::Token`] the
/// `_authToken` line carries the credential; for [`NpmAuth::Oidc`] no token line
/// is written (npm mints a short-lived credential via the OIDC exchange). The
/// caller keeps `cfg_dir` alive across `npm publish`.
pub(crate) fn write_npmrc(
    cfg_dir: &Path,
    registry: &str,
    auth: &NpmAuth,
    access: Option<&str>,
) -> Result<PathBuf> {
    let path = cfg_dir.join(".npmrc");
    let registry_host = registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let mut body = String::new();
    body.push_str(&format!("registry={}\n", registry));
    if let NpmAuth::Token(token) = auth {
        body.push_str(&format!("//{}/:_authToken={}\n", registry_host, token));
    }
    if let Some(a) = access {
        body.push_str(&format!("access={}\n", a));
    }
    write_deterministic(&path, body.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .context("npm: chmod .npmrc to 0600")?;
    }
    Ok(path)
}

/// Top-level publish entrypoint for one `npms[]` entry. Dispatches on
/// [`NpmConfig::mode`].
///
/// Each package whose `npm publish` succeeds is appended to `targets` BEFORE
/// the next publish is attempted, so on a mid-sequence failure the caller
/// still holds the coordinates of every already-live package and can record
/// them for rollback (npm publishes are 72h-irreversible — losing the
/// evidence would orphan a live package). Skip paths append nothing.
pub fn publish_to_npm(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    log: &StageLogger,
    targets: &mut Vec<NpmTarget>,
) -> Result<()> {
    if let Some(skip) = cfg.skip.as_ref() {
        let off = skip
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("npm: render skip template")?;
        if off {
            log.status("skipped npm — skip evaluates true");
            return Ok(());
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        cfg.if_condition.as_deref(),
        "npm publisher",
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status("skipped npm — `if` condition evaluated falsy");
        return Ok(());
    }

    match cfg.mode {
        NpmMode::OptionalDeps => publish_optional_deps(ctx, cfg, crate_name, log, targets),
        NpmMode::Postinstall => publish_postinstall(ctx, cfg, crate_name, log, targets),
    }
}

/// `optional-deps` publish: stage every per-platform package + the metapackage
/// FIRST, then publish them in order (metapackage last so its
/// `optionalDependencies` already resolve).
///
/// Staging up front reads each platform binary and packs its tarball before the
/// first `npm publish` fires, so a missing/unbuilt binary aborts with nothing
/// published instead of half-shipping the earlier platforms to a 72h-
/// irreversible registry. Once publishing begins, each success is pushed onto
/// `targets` immediately so a mid-sequence failure still records the already-
/// live packages for rollback.
fn publish_optional_deps(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    log: &StageLogger,
    targets: &mut Vec<NpmTarget>,
) -> Result<()> {
    let version = ctx.version();
    let registry = resolve_registry(ctx, cfg)?;
    let dist_tag = resolve_tag(ctx, cfg)?;
    let access = resolve_access(cfg);

    // Same graceful-degrade principle as npm AUTH (OIDC↔NPM_TOKEN fallback),
    // applied to the independent PROVENANCE axis: the auth fallback cannot
    // rescue a provenance 422 because publishConfig.provenance:true attaches an
    // attestation regardless of which credential publishes. Gate ONCE for the
    // whole set (per-platform + metapackage) so all share one publishConfig.
    let metapackage = super::optional_deps::resolve_metapackage(cfg, crate_name).to_string();
    let provenance_override = effective_provenance_override(ctx, cfg, &metapackage, log);
    let layout = generate_layout(ctx, cfg, crate_name, &version, provenance_override, log)?;

    if ctx.is_dry_run() {
        if layout.metapackage_files.is_some() {
            log.status(&format!(
                "(dry-run) would publish npm metapackage '{}@{}' + {} platform package(s) to {} (tag={})",
                layout.metapackage,
                version,
                layout.platforms.len(),
                registry,
                dist_tag
            ));
        } else {
            log.status(&format!(
                "(dry-run) would publish {} npm platform package(s) at {} to {} (tag={}) — metapackage '{}' skipped (skip_metapackage)",
                layout.platforms.len(),
                version,
                registry,
                dist_tag,
                layout.metapackage
            ));
        }
        return Ok(());
    }

    // Guard the mutable `latest` pointer BEFORE any irreversible publish: a
    // backfill of a version older than the registry's current `latest`
    // (probed via the metapackage) demotes EVERY package in the family to an
    // inert version-tag so `npm install` is not silently downgraded. Off the
    // dry-run path so dry-run stays network-free.
    let dist_tag =
        dist_tag_guarded_against_regression(&dist_tag, &version, &registry, &metapackage, log);

    let policy = ctx.retry_policy();
    // One sequence-level wall-clock deadline: the loop propagates the first
    // storming package's exhausted-budget Err via `?`, aborting cleanly before
    // the outer job timeout can SIGKILL mid-publish. Successful packages
    // publish in seconds, so the budget effectively bounds the whole sequence.
    let publish_deadline = ctx.retry_deadline();

    // Stage EVERY tarball (per-platform + metapackage) up front, BEFORE the
    // first irreversible `npm publish`. Reading each platform binary and
    // packing its tarball validates that all artifacts exist and assemble
    // cleanly — so a missing binary for platform N aborts here with NOTHING
    // published, rather than half-shipping platforms 1..N-1 to a registry
    // whose unpublish window closes after 72h.
    let mut staged_all: Vec<StagedTarball> = Vec::with_capacity(layout.platforms.len() + 1);
    for plat in &layout.platforms {
        // Embed every binary the package carries — one for a single-command
        // tool, one per command for a multi-command `bins:` package.
        let mut embedded: Vec<(String, Vec<u8>, u32)> = Vec::with_capacity(plat.binaries.len());
        for b in &plat.binaries {
            let binary = fs::read(&b.src)
                .with_context(|| format!("npm: read binary {}", b.src.display()))?;
            embedded.push((b.subpath.clone(), binary, 0o755u32));
        }
        crate::util::guard_no_unrendered(
            ctx,
            log,
            "npm platform package.json",
            &plat.package_json,
        )?;
        staged_all.push(assemble_optional_deps_tarball(
            ctx,
            cfg,
            &plat.name,
            &version,
            &plat.package_json,
            &embedded,
        )?);
    }
    // Metapackage staged last so it publishes last (its optionalDependencies
    // must already resolve at install time). Absent under skip_metapackage —
    // only the per-platform packages ship.
    if let Some(meta) = layout.metapackage_files.as_ref() {
        // One embedded shim per emitted command (single `shim.js` by default,
        // `<command>.js` per `bins` entry).
        let meta_embedded: Vec<(String, Vec<u8>, u32)> = meta
            .shims
            .iter()
            .map(|s| {
                (
                    s.filename.clone(),
                    s.contents.clone().into_bytes(),
                    0o755u32,
                )
            })
            .collect();
        crate::util::guard_no_unrendered(
            ctx,
            log,
            "npm metapackage package.json",
            &meta.package_json,
        )?;
        staged_all.push(assemble_optional_deps_tarball(
            ctx,
            cfg,
            &layout.metapackage,
            &version,
            &meta.package_json,
            &meta_embedded,
        )?);
    }

    // All artifacts validated and staged — now publish in order (per-platform
    // packages first, metapackage last). Auth is resolved PER package (the
    // metapackage may exist with a Trusted Publisher while the sub-packages are
    // brand new), and each success is recorded immediately for rollback before
    // the next attempt.
    for staged in &staged_all {
        if let Some(t) = publish_one_tarball(
            ctx,
            staged,
            &version,
            &registry,
            &dist_tag,
            &access,
            &policy,
            publish_deadline,
            cfg,
            log,
        )? {
            targets.push(t);
        }
    }

    Ok(())
}

/// How postinstall mode treats a configured optional-deps-only field.
#[derive(Clone, Copy)]
enum ModeGate {
    /// Documented silent-ignore — kept for back-compat: these fields predate
    /// the two-mode split and existing postinstall configs may carry them.
    Ignore,
    /// Hard error — silently ignoring the field would ship something other
    /// than what the config asked for (a different package set / naming).
    Error,
}

/// The single mode-gate for optional-deps-only fields in `postinstall` mode.
///
/// One table names every optional-deps-only field with its behavior: the
/// legacy fields (`scope`/`metapackage`/`bin`/`libc_aware`) keep their
/// documented silent-ignore for back-compat, while the two newer fields
/// (`skip_metapackage`/`platform_name_template`) hard-error. The gate
/// evaluates VALUES, not presence: `skip_metapackage: false` (or a template
/// rendering falsey/empty) and an empty/whitespace `platform_name_template`
/// are inert — no error.
fn gate_optional_deps_only_fields(ctx: &Context, cfg: &NpmConfig) -> Result<()> {
    let set = |v: &Option<String>| v.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    let skip_metapackage_active = match cfg.skip_metapackage.as_ref() {
        Some(s) => s
            .try_evaluates_to_true(|t| ctx.render_template(t))
            .context("npm: render skip_metapackage template")?,
        None => false,
    };
    let gates: &[(&str, ModeGate, bool)] = &[
        ("scope", ModeGate::Ignore, set(&cfg.scope)),
        ("metapackage", ModeGate::Ignore, set(&cfg.metapackage)),
        ("bin", ModeGate::Ignore, set(&cfg.bin)),
        // libc_aware defaults true; only an explicit non-default is "set".
        ("libc_aware", ModeGate::Ignore, !cfg.libc_aware),
        ("skip_metapackage", ModeGate::Error, skip_metapackage_active),
        (
            "platform_name_template",
            ModeGate::Error,
            set(&cfg.platform_name_template),
        ),
    ];
    let offending: Vec<&str> = gates
        .iter()
        .filter(|(_, gate, active)| *active && matches!(gate, ModeGate::Error))
        .map(|(name, _, _)| *name)
        .collect();
    if !offending.is_empty() {
        bail!(
            "npm: `{}` only applies to optional-deps mode — postinstall mode \
             publishes one package named by `name:` with no metapackage; \
             remove the field(s) or set mode: optional-deps",
            offending.join("`, `")
        );
    }
    Ok(())
}

/// `postinstall` publish: pack + publish a single download-shim package.
fn publish_postinstall(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    log: &StageLogger,
    targets: &mut Vec<NpmTarget>,
) -> Result<()> {
    gate_optional_deps_only_fields(ctx, cfg)?;
    preflight_multi_format_unambiguous(ctx, cfg, crate_name)?;

    let version = ctx.version();
    let pkg_name = resolve_name(cfg, crate_name).to_string();
    let registry = resolve_registry(ctx, cfg)?;
    let dist_tag = resolve_tag(ctx, cfg)?;
    let access = resolve_access(cfg);

    // The download URL renders against binary_version (else the release
    // version); package.json keeps the npm package version.
    let download_version = super::manifest::resolve_binary_version(ctx, cfg, &version)?;
    let binaries =
        super::manifest::collect_platform_binaries(ctx, cfg, &pkg_name, &download_version, log)?;
    if binaries.is_empty() {
        log.warn(&format!(
            "npm package '{}' has no archive artifacts matching any node platform/cpu pair; \
             nothing to publish",
            pkg_name
        ));
        return Ok(());
    }

    let provenance_override = effective_provenance_override(ctx, cfg, &pkg_name, log);
    let staged = assemble_postinstall_tarball(
        ctx,
        log,
        cfg,
        crate_name,
        &version,
        &binaries,
        provenance_override,
    )?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish npm package '{}@{}' to {} (tag={})",
            staged.package, version, registry, dist_tag
        ));
        return Ok(());
    }

    // Guard the mutable `latest` pointer BEFORE the irreversible publish: a
    // backfill of a version older than the registry's current `latest` demotes
    // this package to an inert version-tag so `npm install` is not silently
    // downgraded. Off the dry-run path so dry-run stays network-free.
    let dist_tag =
        dist_tag_guarded_against_regression(&dist_tag, &version, &registry, &pkg_name, log);

    let policy = ctx.retry_policy();
    let publish_deadline = ctx.retry_deadline();
    if let Some(t) = publish_one_tarball(
        ctx,
        &staged,
        &version,
        &registry,
        &dist_tag,
        &access,
        &policy,
        publish_deadline,
        cfg,
        log,
    )? {
        targets.push(t);
    }
    Ok(())
}

/// Idempotently publish one staged tarball: resolve this package's credential
/// (per-package under [`NpmAuthMode`]), short-circuit when the exact
/// `<name>@<version>` already exists on the registry, else `npm publish` with
/// retry and the `auto`-mode OIDC→token fallback. Returns the recorded
/// [`NpmTarget`].
#[allow(clippy::too_many_arguments)]
fn publish_one_tarball(
    ctx: &Context,
    staged: &StagedTarball,
    version: &str,
    registry: &str,
    dist_tag: &str,
    access: &Option<String>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    cfg: &NpmConfig,
    log: &StageLogger,
) -> Result<Option<NpmTarget>> {
    let (auth, token) = resolve_auth_for_package(ctx, cfg, registry, &staged.package, log)?;
    let cfg_dir = TempDir::new().context("npm: create .npmrc temp dir")?;
    write_npmrc(cfg_dir.path(), registry, &auth, access.as_deref())?;

    if version_already_published(&staged.package, version, cfg_dir.path(), registry, log)? {
        log.status(&format!(
            "skipped '{}@{}' — already published to {} (idempotent re-run)",
            staged.package, version, registry
        ));
        return Ok(Some(NpmTarget {
            package: staged.package.clone(),
            version: version.to_string(),
            registry: registry.to_string(),
            dist_tag: dist_tag.to_string(),
            token_env_var: token_env_var(cfg).to_string(),
        }));
    }

    publish_with_oidc_fallback(
        &staged.package,
        cfg.auth,
        &auth,
        Some(token),
        cfg_dir.path(),
        registry,
        access.as_deref(),
        log,
        &mut |npmrc_dir, npm_auth| {
            run_npm_publish(
                &staged.tarball_path,
                npmrc_dir,
                registry,
                dist_tag,
                access.as_deref(),
                npm_auth,
                policy,
                deadline,
                log,
            )
        },
    )?;

    log.status(&format!(
        "published '{}@{}' to {} (tag={})",
        staged.package, version, registry, dist_tag
    ));

    Ok(Some(NpmTarget {
        package: staged.package.clone(),
        version: version.to_string(),
        registry: registry.to_string(),
        dist_tag: dist_tag.to_string(),
        token_env_var: token_env_var(cfg).to_string(),
    }))
}

/// Run a package's publish, applying the `auto`-mode OIDC→token fallback.
///
/// When the chosen credential is OIDC (Trusted Publishing) and the publish
/// FAILS, `auto` mode retries once with the token — if a token is available —
/// rewriting the `.npmrc` to carry `_authToken`, and emits a LOUD warning
/// naming the package so the operator knows Trusted Publishing was not actually
/// exercised. The release then succeeds via the token. In `oidc` mode there is
/// NO fallback: the failure propagates (fail loud). In `token` mode OIDC is
/// never the chosen credential, so the fallback never triggers.
///
/// `do_publish` is injected so the actual `npm publish` can be stubbed in unit
/// tests; production passes [`run_npm_publish`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_with_oidc_fallback(
    package: &str,
    mode: NpmAuthMode,
    auth: &NpmAuth,
    token: Option<String>,
    cfg_dir: &Path,
    registry: &str,
    access: Option<&str>,
    log: &StageLogger,
    do_publish: &mut dyn FnMut(&Path, &NpmAuth) -> Result<()>,
) -> Result<()> {
    let first = do_publish(cfg_dir, auth);
    if first.is_ok() {
        return Ok(());
    }

    // Fallback applies ONLY when: the chosen credential was OIDC, the mode is
    // `auto` (not strict `oidc`), and a token is available to retry with.
    let token = token.filter(|t| !t.is_empty());
    let oidc_chosen = matches!(auth, NpmAuth::Oidc(_));
    if mode == NpmAuthMode::Auto
        && oidc_chosen
        && let Some(token) = token
    {
        log.warn(&format!(
            "OIDC / Trusted Publishing publish FAILED for '{}'; falling back to NPM_TOKEN — \
             Trusted Publishing was NOT exercised for this package. Verify the package's \
             Trusted Publisher config (registry, repository, workflow).",
            package
        ));
        let token_auth = NpmAuth::Token(token);
        // Rewrite the .npmrc to carry the token line for the retry.
        write_npmrc(cfg_dir, registry, &token_auth, access)?;
        return do_publish(cfg_dir, &token_auth);
    }

    first
}

/// Build the `npm publish` command for one tarball. Under [`NpmAuth::Oidc`] the
/// resolved `ACTIONS_ID_TOKEN_REQUEST_*` pairs are threaded onto the subprocess
/// env so the npm CLI performs the Trusted Publishing token exchange itself; a
/// token credential reaches npm only via the `.npmrc` `--userconfig`, never the
/// subprocess env or argv.
pub(crate) fn build_npm_publish_command(
    tarball: &Path,
    cfg_dir: &Path,
    registry: &str,
    dist_tag: &str,
    access: Option<&str>,
    auth: &NpmAuth,
) -> Command {
    let mut cmd = Command::new("npm");
    cmd.arg("publish")
        .arg(tarball)
        .arg("--userconfig")
        .arg(cfg_dir.join(".npmrc"))
        .arg("--registry")
        .arg(registry)
        .arg("--tag")
        .arg(dist_tag);
    if let Some(a) = access {
        cmd.arg("--access").arg(a);
    }
    if let NpmAuth::Oidc(oidc_env) = auth {
        for (name, value) in oidc_env {
            cmd.env(name, value);
        }
    }
    cmd
}

/// `npm publish <tarball> --userconfig <.npmrc> --registry <url> --tag
/// <dist_tag> [--access <a>]`, wrapped in [`retry_sync_deadline`]. Transient
/// registry failures retry until either the attempt count is exhausted or the
/// optional wall-clock `deadline` (from `retry.max_elapsed`) would be crossed
/// by the next backoff. A token is read from
/// `.npmrc`, never argv; under OIDC the npm CLI mints a short-lived credential
/// from the threaded `ACTIONS_ID_TOKEN_REQUEST_*` env. Transient registry
/// failures retry; others break.
#[allow(clippy::too_many_arguments)]
fn run_npm_publish(
    tarball: &Path,
    cfg_dir: &Path,
    registry: &str,
    dist_tag: &str,
    access: Option<&str>,
    auth: &NpmAuth,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<()> {
    retry_npm_publish(policy, deadline, log, |_attempt| {
        let mut cmd = build_npm_publish_command(tarball, cfg_dir, registry, dist_tag, access, auth);
        log.verbose(&format!(
            "running npm publish {} --registry {} --tag {}",
            tarball.display(),
            registry,
            dist_tag
        ));
        let out = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return Err(ControlFlow::Break(anyhow::Error::from(e).context(format!(
                    "npm: invoke `npm publish` for {}",
                    tarball.display()
                ))));
            }
        };
        if out.status.success() {
            return Ok(());
        }
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let stderr_trimmed = stderr_raw.trim();
        let err = anyhow::anyhow!(
            "npm: `npm publish` exited with status {}: {}",
            out.status,
            anodizer_core::redact::redact_bearer_tokens(stderr_trimmed)
        );
        if is_transient_npm_publish_stderr(stderr_trimmed) {
            Err(ControlFlow::Continue(err))
        } else {
            Err(ControlFlow::Break(err))
        }
    })
}

/// Drive the `npm publish` attempt ladder under [`retry_sync_deadline`],
/// warning once per transient re-attempt and honoring the optional wall-clock
/// `deadline` derived from `retry.max_elapsed`. `attempt_op` performs one
/// publish attempt: `Ok` on success, `ControlFlow::Continue` for a transient
/// failure (retry), `ControlFlow::Break` for a fatal one (stop now). Splitting
/// the ladder from the subprocess build keeps the deadline wiring testable
/// without spawning `npm`.
pub(crate) fn retry_npm_publish<F>(
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
    mut attempt_op: F,
) -> Result<()>
where
    F: FnMut(u32) -> Result<(), ControlFlow<anyhow::Error, anyhow::Error>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut last_attempt = 0u32;
    let mut last_was_continue = false;
    let result = retry_sync_deadline(
        RetryLog::new("npm publish", log),
        policy,
        deadline,
        |attempt| {
            last_attempt = attempt;
            let r = attempt_op(attempt);
            last_was_continue = matches!(r, Err(ControlFlow::Continue(_)));
            r
        },
    );
    // A budget stop is the ONLY way to end with Err + a deadline set + the last
    // op returning Continue + fewer than max_attempts used: attempts-exhausted
    // ends at last_attempt == max_attempts, and a fatal Break sets
    // last_was_continue = false. Distinguish it so the failure reads as
    // resumable rather than a hard npm error.
    if result.is_err() && deadline.is_some() && last_was_continue && last_attempt < max_attempts {
        log.warn(
            "npm retry budget (retry.max_elapsed) exhausted before the job timeout; \
             stopping now — an idempotent re-run resumes from the already-published packages",
        );
    }
    result
}

/// `npm unpublish <package>@<version> --force` invocation used by rollback.
/// Within the 72h window this returns Ok; outside it npm returns non-zero and
/// the call surfaces the "cannot unpublish past 72h" error (warn-only at the
/// caller).
pub(crate) fn run_npm_unpublish(
    package: &str,
    version: &str,
    cfg_dir: &Path,
    registry: &str,
    log: &StageLogger,
) -> Result<()> {
    let mut cmd = Command::new("npm");
    cmd.arg("unpublish")
        .arg(format!("{}@{}", package, version))
        .arg("--userconfig")
        .arg(cfg_dir.join(".npmrc"))
        .arg("--registry")
        .arg(registry)
        .arg("--force");
    log.verbose(&format!(
        "running npm unpublish {}@{} --registry {}",
        package, version, registry
    ));
    let out = cmd
        .output()
        .with_context(|| format!("npm: invoke `npm unpublish` for {}@{}", package, version))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "npm: `npm unpublish` exited with status {}: {}",
            out.status,
            anodizer_core::redact::redact_bearer_tokens(stderr.trim())
        );
    }
    Ok(())
}
