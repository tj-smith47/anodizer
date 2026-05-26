//! NPM publish orchestration — assembles the tarball and invokes
//! `npm publish` with a per-run `.npmrc` that carries the auth token.
//!
//! Token handling:
//!   * The token is resolved from `cfg.token` (templated) or the
//!     `NPM_TOKEN` env var. It is **never** placed on the `npm publish`
//!     argv — npm reads `_authToken` from `.npmrc`.
//!   * Each publish writes a process-private `.npmrc` to a `tempfile::
//!     TempDir` and passes `--userconfig <path>` to `npm publish`. The
//!     temp dir is dropped after the publish completes, removing the
//!     `.npmrc` from disk.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::config::NpmConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use tempfile::TempDir;

use super::manifest::{
    PlatformBinary, render_launcher_js, render_package_json, render_postinstall_js, resolve_access,
    resolve_extra_files, resolve_name, resolve_registry, resolve_tag, token_env_var,
};

/// Outcome of [`publish_to_npm`]: an [`NpmTarget`] when the publish path
/// actually pushed a tarball, `None` for every skip path (skip=true /
/// dry-run / missing token / `if:` falsy). The caller uses the
/// `Option<NpmTarget>` to gate rollback-evidence recording so a
/// `--rollback-only` cannot try to `npm unpublish` a package the run
/// never pushed.
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

/// Result of [`assemble_tarball`] — the path to the produced `.tgz` and
/// the staging temp dir kept alive so the tarball remains on disk for
/// `npm publish`. Drop the `_staging` field to delete the staging dir.
pub struct StagedTarball {
    /// Staging temp dir holding the rendered package + tarball.
    _staging: TempDir,
    /// Path to the `<name>-<version>.tgz` inside the staging dir.
    pub tarball_path: PathBuf,
    /// Resolved package name (scoped or unscoped).
    pub package: String,
}

/// Assemble the npm tarball: write `package.json`, `postinstall.js`,
/// `bin/<name>.js`, and any `extra_files` into a staging `package/`
/// directory, then tar+gzip it. The tarball layout matches npm's
/// expectations (a single top-level `package/` directory).
///
/// Determinism: every file is written with a fixed mtime / mode so
/// repeated runs produce byte-identical tarballs.
pub fn assemble_tarball(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    version: &str,
    binaries: &[PlatformBinary],
) -> Result<StagedTarball> {
    let staging = TempDir::new().context("npm: create staging dir")?;
    let pkg_dir = staging.path().join("package");
    fs::create_dir_all(&pkg_dir).context("npm: create package/ in staging dir")?;

    let pkg_name = resolve_name(cfg, crate_name).to_string();
    let pkg_json = render_package_json(ctx, cfg, &pkg_name, version, binaries)?;
    write_deterministic(&pkg_dir.join("package.json"), pkg_json.as_bytes())?;

    let postinstall = render_postinstall_js(&pkg_name);
    write_deterministic(&pkg_dir.join("postinstall.js"), postinstall.as_bytes())?;

    fs::create_dir_all(pkg_dir.join("bin")).context("npm: create package/bin in staging dir")?;
    let launcher = render_launcher_js(&pkg_name);
    let launcher_basename = pkg_name.rsplit('/').next().unwrap_or(&pkg_name);
    write_deterministic(
        &pkg_dir
            .join("bin")
            .join(format!("{}.js", launcher_basename)),
        launcher.as_bytes(),
    )?;

    // Copy extra_files matching the configured glob set. Globs are resolved
    // relative to the workspace root (cwd); files outside the workspace
    // are silently skipped.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for pattern in resolve_extra_files(cfg) {
        let absolute_pattern = if Path::new(&pattern).is_absolute() {
            pattern.clone()
        } else {
            cwd.join(&pattern).to_string_lossy().into_owned()
        };
        let entries = match glob::glob(&absolute_pattern) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if !entry.is_file() {
                continue;
            }
            let basename = match entry.file_name() {
                Some(n) => n,
                None => continue,
            };
            let dst = pkg_dir.join(basename);
            let bytes = match fs::read(&entry) {
                Ok(b) => b,
                Err(_) => continue,
            };
            write_deterministic(&dst, &bytes)?;
        }
    }

    // Pack the tarball. We use the `tar` + `flate2` crates (already in
    // the workspace via stage-archive / sibling stages); for the npm
    // publisher we shell out to `tar` to keep the tarball assembly
    // implementation light and consistent with the rest of stage-publish
    // (chocolatey similarly shells out for nupkg packing semantics).
    let tarball_name = format!(
        "{}-{}.tgz",
        // npm tarball names use the unscoped basename for scoped
        // packages (e.g. `@scope/foo-1.2.3.tgz` would be invalid).
        sanitize_tarball_basename(&pkg_name),
        version
    );
    let tarball_path = staging.path().join(&tarball_name);
    pack_tarball(&pkg_dir, &tarball_path)?;

    Ok(StagedTarball {
        _staging: staging,
        tarball_path,
        package: pkg_name,
    })
}

/// Encode a package name into a tarball-basename-safe form: scoped
/// `@org/name` collapses to `org-name`, unscoped `name` stays as-is.
fn sanitize_tarball_basename(pkg_name: &str) -> String {
    if let Some(rest) = pkg_name.strip_prefix('@') {
        rest.replace('/', "-")
    } else {
        pkg_name.to_string()
    }
}

/// Write `bytes` to `path` with a deterministic mode (`0o644`) and mtime
/// (the SDE / unix-epoch sentinel). Used for every file rendered into the
/// tarball staging dir so the resulting `.tgz` is byte-identical across
/// runs that produced the same logical contents.
fn write_deterministic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("npm: create parent of {}", path.display()))?;
    }
    let mut f =
        fs::File::create(path).with_context(|| format!("npm: create {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("npm: write {}", path.display()))?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.ends_with(".js"))
            .unwrap_or(false)
        {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).ok();
    }
    Ok(())
}

/// Pack the `package/` directory at `pkg_dir` into `tarball_path` (a
/// `.tgz`). Uses the `tar` crate + `flate2` for in-process tarball
/// assembly with deterministic mtimes/modes (no subprocess required).
fn pack_tarball(pkg_dir: &Path, tarball_path: &Path) -> Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let f = fs::File::create(tarball_path)
        .with_context(|| format!("npm: create tarball {}", tarball_path.display()))?;
    let enc = GzEncoder::new(f, Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.mode(tar::HeaderMode::Deterministic);
    builder
        .append_dir_all("package", pkg_dir)
        .context("npm: append package/ to tarball")?;
    builder
        .into_inner()
        .context("npm: finalize tar builder")?
        .finish()
        .context("npm: finalize gzip stream")?;
    Ok(())
}

/// Resolve the auth token for this entry: `cfg.token` (templated)
/// takes precedence, then the `NPM_TOKEN` env var. Returns an empty
/// string when both are unset/empty — the caller surfaces a clear
/// "missing token" error rather than silently invoking `npm publish`
/// against an anonymous session.
pub(crate) fn resolve_token(ctx: &Context, cfg: &NpmConfig) -> Result<String> {
    if let Some(raw) = cfg.token.as_deref()
        && !raw.is_empty()
    {
        let rendered = ctx
            .render_template(raw)
            .context("npm: render token template")?;
        if !rendered.is_empty() {
            return Ok(rendered);
        }
    }
    let env = ctx.env_source();
    Ok(env.var(token_env_var(cfg)).unwrap_or_default().to_string())
}

/// Write a per-run `.npmrc` carrying `_authToken=...` for the configured
/// registry. The file lives under `cfg_dir` (a `TempDir` the caller
/// must keep alive across the `npm publish` invocation) so the token is
/// removed from disk immediately after publish.
pub(crate) fn write_npmrc(
    cfg_dir: &Path,
    registry: &str,
    token: &str,
    access: Option<&str>,
) -> Result<PathBuf> {
    let path = cfg_dir.join(".npmrc");
    // npm's auth resolution keys off the registry hostname (sans scheme).
    let registry_host = registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let mut body = String::new();
    body.push_str(&format!("registry={}\n", registry));
    body.push_str(&format!("//{}/:_authToken={}\n", registry_host, token));
    if let Some(a) = access {
        body.push_str(&format!("access={}\n", a));
    }
    body.push_str("always-auth=true\n");
    write_deterministic(&path, body.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // .npmrc carries a credential — narrow to 0600.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .context("npm: chmod .npmrc to 0600")?;
    }
    Ok(path)
}

/// Top-level publish entrypoint for one `npms[]` entry.
///
/// Returns `Ok(Some(target))` after a successful push, `Ok(None)` for
/// every skip path (skip=true / disable=true / dry-run / missing token
/// when not required). Errors bubble for unexpected failures (e.g. the
/// tarball assembly failed, or `npm publish` exited non-zero).
pub fn publish_to_npm(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<NpmTarget>> {
    // ---- Skip gate ----
    if let Some(skip) = cfg.skip.as_ref() {
        let off = skip
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("npm: render skip template")?;
        if off {
            log.status("npm: skipping — skip evaluates true");
            return Ok(None);
        }
    }
    if let Some(disable) = cfg.disable.as_ref() {
        let off = disable
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("npm: render disable template")?;
        if off {
            log.status("npm: skipping — disable evaluates true");
            return Ok(None);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        cfg.if_condition.as_deref(),
        "npm publisher",
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status("npm: skipping — `if` condition evaluated falsy");
        return Ok(None);
    }

    let version = ctx.version();
    let pkg_name = resolve_name(cfg, crate_name).to_string();
    let registry = resolve_registry(cfg);
    let dist_tag = resolve_tag(cfg).to_string();
    let access = resolve_access(cfg);

    let binaries = super::manifest::collect_platform_binaries(ctx, cfg, &pkg_name, &version)?;
    if binaries.is_empty() {
        log.warn(&format!(
            "npm: '{}' has no archive artifacts matching any node platform/cpu pair; \
             nothing to publish",
            pkg_name
        ));
        return Ok(None);
    }

    let staged = assemble_tarball(ctx, cfg, crate_name, &version, &binaries)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish npm package '{}@{}' to {} (tag={})",
            staged.package, version, registry, dist_tag
        ));
        return Ok(None);
    }

    let token = resolve_token(ctx, cfg)?;
    if token.is_empty() {
        bail!(
            "npm: NPM_TOKEN env var (or cfg.token) is required to publish '{}@{}' to {}",
            staged.package,
            version,
            registry
        );
    }

    let cfg_dir = TempDir::new().context("npm: create .npmrc temp dir")?;
    write_npmrc(cfg_dir.path(), &registry, &token, access.as_deref())?;

    run_npm_publish(
        &staged.tarball_path,
        cfg_dir.path(),
        &registry,
        &dist_tag,
        access.as_deref(),
        log,
    )?;

    Ok(Some(NpmTarget {
        package: staged.package,
        version,
        registry,
        dist_tag,
        token_env_var: token_env_var(cfg).to_string(),
    }))
}

/// Invoke `npm publish <tarball> --userconfig <.npmrc> --registry <url>
/// --tag <dist_tag> [--access <a>]`. Token is read from `.npmrc`,
/// never argv. Surfaces a clean error on non-zero exit; stderr is
/// scrubbed for bearer-token redaction.
fn run_npm_publish(
    tarball: &Path,
    cfg_dir: &Path,
    registry: &str,
    dist_tag: &str,
    access: Option<&str>,
    log: &StageLogger,
) -> Result<()> {
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
    log.status(&format!(
        "npm: publish {} --registry {} --tag {}",
        tarball.display(),
        registry,
        dist_tag
    ));
    let out = cmd
        .output()
        .with_context(|| format!("npm: invoke `npm publish` for {}", tarball.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "npm: `npm publish` exited with status {}: {}",
            out.status,
            anodizer_core::redact::redact_bearer_tokens(stderr.trim())
        );
    }
    Ok(())
}

/// `npm unpublish <package>@<version> --userconfig <.npmrc> --registry
/// <url> --force` invocation used by [`crate::npm::publisher::NpmPublisher::rollback`].
/// Within the 72h window this returns Ok; outside the window npm returns
/// a non-zero exit and a "cannot unpublish past 72h" error which we
/// surface to the operator (warn-only — rollback does not bubble Err).
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
    log.status(&format!(
        "npm: unpublish {}@{} --registry {}",
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
