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
use std::io::Write;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::config::{
    ArchivesConfig, NpmAuthMode, NpmConfig, NpmMode, NpmTemplatedExtraFile,
};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, RetryPolicy, retry_sync_deadline};
use anodizer_core::template_file_render::render_templated_file_entry;
use anyhow::{Context as _, Result, bail};
use tempfile::TempDir;

use super::manifest::{
    DEFAULT_TAG, PlatformBinary, effective_provenance_override, render_launcher_js,
    render_package_json, render_postinstall_js, resolve_access, resolve_extra_files, resolve_name,
    resolve_registry, resolve_tag, token_env_var,
};
use super::optional_deps::generate_layout;

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

/// Result of [`assemble_postinstall_tarball`] — the produced `.tgz` and the
/// staging temp dir kept alive so the tarball stays on disk for `npm publish`.
pub struct StagedTarball {
    /// Staging temp dir holding the rendered package + tarball.
    _staging: TempDir,
    /// Path to the `<name>-<version>.tgz` inside the staging dir.
    pub tarball_path: PathBuf,
    /// Resolved package name (scoped or unscoped).
    pub package: String,
}

/// Assemble the postinstall-mode npm tarball: write `package.json`,
/// `postinstall.js`, `bin/<name>.js`, and any `extra_files` into a staging
/// `package/` directory, then tar+gzip it. Every file is written with a fixed
/// mode/mtime so repeated runs produce byte-identical tarballs.
///
/// `provenance_override` is forwarded to [`render_package_json`] so the live
/// publish can downgrade `publishConfig.provenance` on a runner that cannot
/// mint an npm attestation.
pub fn assemble_postinstall_tarball(
    ctx: &Context,
    log: &StageLogger,
    cfg: &NpmConfig,
    crate_name: &str,
    version: &str,
    binaries: &[PlatformBinary],
    provenance_override: Option<bool>,
) -> Result<StagedTarball> {
    let staging = TempDir::new().context("npm: create staging dir")?;
    let pkg_dir = staging.path().join("package");
    fs::create_dir_all(&pkg_dir).context("npm: create package/ in staging dir")?;

    let pkg_name = resolve_name(cfg, crate_name).to_string();
    let pkg_json = render_package_json(
        ctx,
        cfg,
        &pkg_name,
        crate_name,
        version,
        binaries,
        provenance_override,
    )?;
    crate::util::guard_no_unrendered(ctx, log, "npm package.json", &pkg_json)?;
    write_deterministic(&pkg_dir.join("package.json"), pkg_json.as_bytes())?;

    // Resolve the command set once: the postinstall script extracts every
    // target binary and each command gets its own `bin/<command>.js` launcher.
    let commands = super::manifest::postinstall_commands(cfg, &pkg_name);
    let targets: Vec<String> = commands.iter().map(|(_, t)| t.clone()).collect();

    let postinstall = render_postinstall_js(&targets);
    write_deterministic(&pkg_dir.join("postinstall.js"), postinstall.as_bytes())?;

    fs::create_dir_all(pkg_dir.join("bin")).context("npm: create package/bin in staging dir")?;
    for (command, target) in &commands {
        let launcher = render_launcher_js(cfg, command, target);
        write_deterministic(
            &pkg_dir.join("bin").join(format!("{}.js", command)),
            launcher.as_bytes(),
        )?;
    }

    copy_extra_files(cfg, &pkg_dir)?;
    render_templated_extra_files(ctx, cfg, &pkg_dir)?;

    let tarball_name = format!("{}-{}.tgz", sanitize_tarball_basename(&pkg_name), version);
    let tarball_path = staging.path().join(&tarball_name);
    // Postinstall mode embeds no executable binary on disk (the launcher and
    // postinstall are `.js`, handled by pack_tarball's `.js` → 0o755 rule).
    pack_tarball(&pkg_dir, &tarball_path, &std::collections::BTreeSet::new())?;

    Ok(StagedTarball {
        _staging: staging,
        tarball_path,
        package: pkg_name,
    })
}

/// Assemble one `optional-deps` package (per-platform OR metapackage) into a
/// staging `package/` dir and pack it to a `.tgz`. Per-platform packages embed
/// the binary at mode `0o755`; the metapackage embeds `shim.js`. `extra_files`
/// (README/LICENSE) are copied into both.
pub fn assemble_optional_deps_tarball(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    version: &str,
    package_json: &str,
    embedded: &[(String, Vec<u8>, u32)],
) -> Result<StagedTarball> {
    let staging = TempDir::new().context("npm: create staging dir")?;
    let pkg_dir = staging.path().join("package");
    fs::create_dir_all(&pkg_dir).context("npm: create package/ in staging dir")?;

    write_deterministic(&pkg_dir.join("package.json"), package_json.as_bytes())?;
    // Capture each embedded entry's intended exec bit HERE (from the caller's
    // mode, not the host fs) so pack_tarball can stamp 0o755 even on a Windows
    // build host where `write_with_mode` can't set the on-disk exec bit.
    let mut executables = std::collections::BTreeSet::new();
    for (name, bytes, mode) in embedded {
        write_with_mode(&pkg_dir.join(name), bytes, *mode)?;
        if mode & 0o111 != 0 {
            executables.insert(name.replace('\\', "/"));
        }
    }
    copy_extra_files(cfg, &pkg_dir)?;
    render_templated_extra_files(ctx, cfg, &pkg_dir)?;

    let tarball_name = format!("{}-{}.tgz", sanitize_tarball_basename(pkg_name), version);
    let tarball_path = staging.path().join(&tarball_name);
    pack_tarball(&pkg_dir, &tarball_path, &executables)?;

    Ok(StagedTarball {
        _staging: staging,
        tarball_path,
        package: pkg_name.to_string(),
    })
}

/// Copy `extra_files`-matching files (README/LICENSE globs) into `pkg_dir`.
fn copy_extra_files(cfg: &NpmConfig, pkg_dir: &Path) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for pattern in resolve_extra_files(cfg) {
        let absolute_pattern = if Path::new(&pattern).is_absolute() {
            pattern.clone()
        } else {
            cwd.join(&pattern).to_string_lossy().into_owned()
        };
        let entries = glob::glob(&absolute_pattern)
            .with_context(|| format!("npm: invalid extra_files glob pattern '{}'", pattern))?;
        for entry in entries.flatten() {
            if !entry.is_file() {
                continue;
            }
            let basename = match entry.file_name() {
                Some(n) => n,
                None => continue,
            };
            let dst = pkg_dir.join(basename);
            // A file matched the declared extra_files glob but can't be read:
            // publishing without it would silently drop a declared
            // README/LICENSE, so surface the failure instead of skipping.
            let bytes = fs::read(&entry)
                .with_context(|| format!("npm: read extra_files entry '{}'", entry.display()))?;
            write_deterministic(&dst, &bytes)?;
        }
    }
    Ok(())
}

/// Render `templated_extra_files` entries into `pkg_dir` via the shared
/// template-file pipeline (skip / render-src / render-dst / traversal-reject).
fn render_templated_extra_files(ctx: &Context, cfg: &NpmConfig, pkg_dir: &Path) -> Result<()> {
    if let Some(specs) = cfg.templated_extra_files.as_ref() {
        for (idx, spec) in specs.iter().enumerate() {
            let bridged = npm_to_template_file_config(spec);
            let label = format!("npm: templated_extra_files[{}]", idx);
            let render = match render_templated_file_entry(ctx, &bridged, &label)? {
                Some(r) => r,
                None => continue,
            };
            let dst_path = pkg_dir.join(&render.rendered_dst);
            write_deterministic(&dst_path, render.rendered_contents.as_bytes())?;
        }
    }
    Ok(())
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

/// Bridge an [`NpmTemplatedExtraFile`] into the shared
/// [`anodizer_core::config::TemplateFileConfig`] consumed by
/// [`render_templated_file_entry`].
fn npm_to_template_file_config(
    spec: &NpmTemplatedExtraFile,
) -> anodizer_core::config::TemplateFileConfig {
    anodizer_core::config::TemplateFileConfig {
        id: None,
        src: spec.src.clone(),
        dst: spec.dst.clone(),
        mode: None,
        skip: None,
    }
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

/// Probe the registry for an existing `<name>@<version>` publication via
/// `npm view`.
///
/// Returns `Ok(true)` when the version is already published, `Ok(false)` only
/// on a definitive `E404` (the package/version genuinely does not exist).
///
/// Fail-closed on an inconclusive probe: a spawn failure or any non-404 error
/// shape (registry 5xx, auth failure, network glitch) surfaces an `Err`
/// rather than `Ok(false)`. An `npm publish` is irreversible after npm's 72h
/// unpublish window, so a probe that *cannot prove* the version is absent must
/// not green-light the publish — assuming "not published" on an outage would
/// re-push over an existing version (or double-ship) the moment the registry
/// recovers. The caller aborts this package's publish and records the failure
/// for the operator instead.
pub(crate) fn version_already_published(
    name: &str,
    version: &str,
    cfg_dir: &Path,
    registry: &str,
    log: &StageLogger,
) -> Result<bool> {
    let mut cmd = Command::new("npm");
    cmd.arg("view")
        .arg(format!("{}@{}", name, version))
        .arg("version")
        .arg("--registry")
        .arg(registry)
        .arg("--userconfig")
        .arg(cfg_dir.join(".npmrc"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            log.warn(&format!(
                "could not probe npm for '{}@{}' on {} (spawn failed: {}); \
                 refusing to publish blind — fix the npm CLI and retry",
                name, version, registry, e
            ));
            bail!(
                "npm: idempotency probe for '{}@{}' failed to spawn npm view",
                name,
                version
            );
        }
    };
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Ok(!stdout.trim().is_empty());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("E404") {
        return Ok(false);
    }
    log.warn(&format!(
        "npm idempotency probe for '{}@{}' on {} was inconclusive (not a 404): {}; \
         refusing to publish blind to a 72h-irreversible registry — retry once the \
         registry is healthy",
        name,
        version,
        registry,
        anodizer_core::redact::redact_bearer_tokens(stderr.trim())
    ));
    bail!(
        "npm: idempotency probe for '{}@{}' returned an inconclusive non-404 error",
        name,
        version
    );
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

/// Write `bytes` to `path` with a deterministic mode (`.js` → `0o755`, else
/// `0o644`) so the resulting `.tgz` is byte-identical across runs.
fn write_deterministic(path: &Path, bytes: &[u8]) -> Result<()> {
    let is_js = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.ends_with(".js"))
        .unwrap_or(false);
    let mode = if is_js { 0o755 } else { 0o644 };
    write_with_mode(path, bytes, mode)
}

/// Write `bytes` to `path` with an explicit unix mode.
fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
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
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).ok();
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Pack the `package/` directory into a `.tgz` with deterministic
/// mtimes/modes (no subprocess).
///
/// Modes are assigned EXPLICITLY (not read from the on-disk file): every path
/// in `executables` (relative to `pkg_dir`, forward-slash normalized) and every
/// `.js` shim is `0o755`, all else `0o644`. This reproduces the staging writers'
/// intended modes WITHOUT depending on the host filesystem — `write_with_mode`
/// only sets the exec bit under `#[cfg(unix)]`, so a Windows build host would
/// otherwise ship the embedded binary at `0o644`. Sorting the walk by relative
/// path + zeroing mtime/uid/gid keeps the tarball byte-identical across runs and
/// across build hosts.
fn pack_tarball(
    pkg_dir: &Path,
    tarball_path: &Path,
    executables: &std::collections::BTreeSet<String>,
) -> Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    // Recursively collect every regular file under `pkg_dir`, keyed by its
    // forward-slash path relative to `pkg_dir`. Sorted below for determinism —
    // `read_dir` order is filesystem-dependent.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(pkg_dir, pkg_dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let f = fs::File::create(tarball_path)
        .with_context(|| format!("npm: create tarball {}", tarball_path.display()))?;
    let enc = GzEncoder::new(f, Compression::default());
    let mut builder = tar::Builder::new(enc);

    for (rel, abs) in &files {
        let bytes =
            fs::read(abs).with_context(|| format!("npm: read staged file {}", abs.display()))?;
        let mode = if executables.contains(rel) || rel.ends_with(".js") {
            0o755
        } else {
            0o644
        };
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(mode);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("package/{rel}"), &bytes[..])
            .with_context(|| format!("npm: append package/{rel} to tarball"))?;
    }

    builder
        .into_inner()
        .context("npm: finalize tar builder")?
        .finish()
        .context("npm: finalize gzip stream")?;
    Ok(())
}

/// Recursively collect every regular file under `dir`, pushing
/// `(relative-to-`base`-forward-slash path, absolute path)` pairs onto `out`.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("npm: read staging dir {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("npm: read entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("npm: stat {}", path.display()))?;
        if file_type.is_dir() {
            collect_files(base, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Resolve the auth token: `cfg.token` (templated) precedence, then the
/// `NPM_TOKEN` env var. Empty when both are unset — the caller surfaces a
/// clear "missing token" error.
pub(crate) fn resolve_token(ctx: &Context, cfg: &NpmConfig) -> Result<String> {
    // The shared ladder filters empties at every rung: an exported-but-blank
    // `NPM_TOKEN` (GitHub Actions' shape for a missing secret) resolves to
    // absent rather than `""`, closing the gap `unwrap_or_default()` left.
    crate::publisher_helpers::resolve_token_with_ladder(
        ctx,
        cfg.token.as_deref(),
        "npm: render token template",
        &[token_env_var(cfg)],
    )
}

/// The two GitHub Actions OIDC request variables npm's Trusted Publishing
/// exchange consumes. Both must be present for an OIDC context to exist — the
/// URL is the token-mint endpoint, the token authorizes the mint request.
pub(crate) const OIDC_ENV_VARS: [&str; 2] = [
    "ACTIONS_ID_TOKEN_REQUEST_URL",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
];

/// Resolved npm publish credential. Exactly one variant authorizes a publish;
/// there is no anonymous variant by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NpmAuth {
    /// A long-lived registry token (`NPM_TOKEN` / `cfg.token`). Written as
    /// `_authToken` into the per-run `.npmrc`.
    Token(String),
    /// A GitHub Actions OIDC context (Trusted Publishing). Carries the
    /// `ACTIONS_ID_TOKEN_REQUEST_*` pairs to thread into the `npm publish`
    /// subprocess so the npm CLI mints a short-lived credential itself; the
    /// `.npmrc` carries no token line.
    Oidc(Vec<(String, String)>),
}

/// Snapshot the GitHub Actions OIDC request env when BOTH variables are present
/// and non-empty, returning every entry to thread into the publish subprocess.
/// Returns `None` (no OIDC context) when either variable is missing/empty.
fn resolve_oidc_env(ctx: &Context) -> Option<Vec<(String, String)>> {
    let env = ctx.env_source();
    let mut out = Vec::with_capacity(OIDC_ENV_VARS.len());
    for name in OIDC_ENV_VARS {
        let val = env.var(name).filter(|v| !v.is_empty())?;
        out.push((name.to_string(), val));
    }
    Some(out)
}

/// Whether a package already exists on the registry, used to drive per-package
/// auth selection in [`NpmAuthMode::Auto`]. `Unknown` is returned when the
/// existence probe could not reach a verdict (network error) — the decision
/// then prefers the safe path rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageExistence {
    /// Registry returned 200 — the package name is already published.
    Exists,
    /// Registry returned 404 — the package name is brand new.
    New,
    /// The probe failed (network/registry error) — existence is undetermined.
    Unknown,
}

/// The credential a per-package auth decision selects, as a pure outcome that
/// carries no secret material (the caller materializes the actual
/// [`NpmAuth`] from it). `FailNewNeedsToken` and `ErrorNoAuth` are terminal —
/// the package cannot be published with the inputs given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthDecision {
    /// Authenticate with the token.
    Token,
    /// Authenticate with OIDC (Trusted Publishing).
    Oidc,
    /// New package + OIDC-only context + no token: Trusted Publishing cannot
    /// create a non-existent package, so the initial publish needs a token.
    FailNewNeedsToken,
    /// No credential is available at all.
    ErrorNoAuth,
}

/// Decide a single package's publish credential from the four facts that govern
/// it: the configured [`NpmAuthMode`], whether the package already exists, and
/// whether an OIDC context / a token are available. Pure — no I/O, no secrets —
/// so the full decision matrix is unit-testable in isolation.
///
/// `auto` semantics (per package):
///
/// | exists?  | OIDC? | token? | decision           |
/// |----------|-------|--------|--------------------|
/// | new      | any   | yes    | `Token`            |
/// | new      | yes   | no     | `FailNewNeedsToken`|
/// | new      | no    | no     | `ErrorNoAuth`      |
/// | exists   | yes   | any    | `Oidc`             |
/// | exists   | no    | yes    | `Token`            |
/// | exists   | no    | no     | `ErrorNoAuth`      |
/// | unknown  | any   | yes    | `Token` (safe)     |
/// | unknown  | yes   | no     | `Oidc` (best effort)|
/// | unknown  | no    | no     | `ErrorNoAuth`      |
///
/// `token` mode forces [`AuthDecision::Token`] (or `ErrorNoAuth` if no token);
/// `oidc` mode forces [`AuthDecision::Oidc`] (or `ErrorNoAuth` if no OIDC
/// context) — strict Trusted-Publishing-only, no token fallback.
pub(crate) fn decide_auth(
    mode: NpmAuthMode,
    existence: PackageExistence,
    oidc_available: bool,
    token_available: bool,
) -> AuthDecision {
    match mode {
        NpmAuthMode::Token => {
            if token_available {
                AuthDecision::Token
            } else {
                AuthDecision::ErrorNoAuth
            }
        }
        NpmAuthMode::Oidc => {
            if oidc_available {
                AuthDecision::Oidc
            } else {
                AuthDecision::ErrorNoAuth
            }
        }
        NpmAuthMode::Auto => match existence {
            PackageExistence::New => {
                if token_available {
                    AuthDecision::Token
                } else if oidc_available {
                    // Trusted Publishing cannot create a package that does not
                    // yet exist — surface a specific, fixable error.
                    AuthDecision::FailNewNeedsToken
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
            PackageExistence::Exists => {
                if oidc_available {
                    AuthDecision::Oidc
                } else if token_available {
                    AuthDecision::Token
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
            PackageExistence::Unknown => {
                if token_available {
                    // Safe path on an inconclusive probe: a token can publish
                    // whether the package exists or not.
                    AuthDecision::Token
                } else if oidc_available {
                    AuthDecision::Oidc
                } else {
                    AuthDecision::ErrorNoAuth
                }
            }
        },
    }
}

/// URL-encode an npm package name for a registry metadata GET: a scoped name's
/// single `/` becomes `%2F` (`@a/b` → `@a%2Fb`); all other characters in valid
/// npm names (lowercase, digits, `-._@`) are already URL-safe.
pub(crate) fn encode_package_path(name: &str) -> String {
    name.replace('/', "%2F")
}

/// Probe the registry for a package's *existence* (any version) via a metadata
/// GET to `<registry>/<url-encoded name>`. 200 → [`PackageExistence::Exists`],
/// 404 → [`PackageExistence::New`]; any transport error or other status →
/// [`PackageExistence::Unknown`] (the caller's `auto` decision then prefers the
/// safe path). This is distinct from [`version_already_published`], which
/// probes for one specific *version* to drive idempotent re-runs.
pub(crate) fn probe_package_existence(
    registry: &str,
    name: &str,
    log: &StageLogger,
) -> PackageExistence {
    let base = registry.trim_end_matches('/');
    let url = format!("{}/{}", base, encode_package_path(name));
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(15)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "npm: could not build HTTP client to probe '{}' existence ({}); \
                 treating existence as unknown",
                name, e
            ));
            return PackageExistence::Unknown;
        }
    };
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            if status.as_u16() == 404 {
                PackageExistence::New
            } else if status.is_success() {
                PackageExistence::Exists
            } else {
                log.warn(&format!(
                    "npm: existence probe for '{}' returned HTTP {} (inconclusive); \
                     treating existence as unknown",
                    name, status
                ));
                PackageExistence::Unknown
            }
        }
        Err(e) => {
            log.warn(&format!(
                "npm: existence probe for '{}' failed ({}); treating existence as unknown",
                name, e
            ));
            PackageExistence::Unknown
        }
    }
}

/// Probe the registry for a package's current `latest` dist-tag via a metadata
/// GET to `<registry>/<url-encoded name>`, reading `.dist-tags.latest`. Returns
/// `None` on a 404 (brand-new package — nothing to regress), any transport /
/// decode error, or an absent tag. Every `None` path FAILS OPEN (the caller
/// keeps the configured tag): a missing signal must never block a legitimate
/// publish.
pub(crate) fn probe_dist_tag_latest(
    registry: &str,
    name: &str,
    log: &StageLogger,
) -> Option<String> {
    let base = registry.trim_end_matches('/');
    let url = format!("{}/{}", base, encode_package_path(name));
    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(15)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "npm: could not build HTTP client to probe '{}' latest dist-tag ({}); \
                 leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => {
            log.warn(&format!(
                "npm: latest-tag probe for '{}' failed ({}); leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    // 404 = brand-new package (no `latest` to regress); any other non-2xx is
    // inconclusive. Both fail open.
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            log.warn(&format!(
                "npm: could not decode metadata for '{}' ({}); leaving the configured tag unguarded",
                name, e
            ));
            return None;
        }
    };
    body.get("dist-tags")
        .and_then(|d| d.get("latest"))
        .and_then(|l| l.as_str())
        .map(str::to_string)
}

/// Guard the mutable `latest` dist-tag against a version REGRESSION.
///
/// npm's `latest` is the tag `npm install <pkg>` (no version) resolves, and
/// publishing an OLDER version with `--tag latest` moves that pointer BACKWARD —
/// silently downgrading every default install. This bites a BACKFILL: completing
/// an interrupted older release after a newer one already published would drag
/// `latest` back to the older version.
///
/// When the configured tag is the default `latest` AND `publish_version` is
/// strictly LOWER than the registry's current `latest`, this returns an INERT
/// named dist-tag `release-<version>`: the version still publishes (versions are
/// immutable and always land), but the `latest` pointer is left on the newer
/// release. Every non-regressing case returns the configured tag unchanged — a
/// NON-default configured tag (the operator asked for an explicit tag),
/// `registry_latest == None` (fail-open), an equal/newer version, or a version
/// string that does not parse as semver.
///
/// The demoted tag is `release-<version>`, NOT the bare version: npm rejects any
/// `--tag` that parses as a semver range (`npm publish`:
/// `if (semver.validRange(tag)) throw "Tag name must not be a valid SemVer
/// range"`), and node-semver strips a leading `v`, so neither `0.19.0` nor
/// `v0.19.0` is a legal tag. The `release-` prefix makes the whole string
/// unparseable as a range while staying per-version, so sequential backfills
/// (0.19 → 0.20 → 0.21) never contend over one shared pointer.
pub(crate) fn guard_latest_regression(
    configured_tag: &str,
    publish_version: &str,
    registry_latest: Option<&str>,
) -> String {
    if configured_tag != DEFAULT_TAG {
        return configured_tag.to_string();
    }
    if let Some(current) = registry_latest {
        if let (Ok(pubv), Ok(cur)) = (
            anodizer_core::git::parse_semver(publish_version),
            anodizer_core::git::parse_semver(current),
        ) {
            if pubv < cur {
                return format!("release-{publish_version}");
            }
        }
    }
    configured_tag.to_string()
}

/// Apply [`guard_latest_regression`] against the live registry: when `dist_tag`
/// is the default `latest`, probe `package`'s current `latest` and demote to an
/// inert version-tag if `version` would regress it. A no-op (returns `dist_tag`
/// verbatim) for any explicit tag, so the network round-trip only happens when a
/// regression is actually possible. Emits a `status` line when it demotes so the
/// operator sees why `latest` was left untouched.
pub(crate) fn dist_tag_guarded_against_regression(
    dist_tag: &str,
    version: &str,
    registry: &str,
    package: &str,
    log: &StageLogger,
) -> String {
    if dist_tag != DEFAULT_TAG {
        return dist_tag.to_string();
    }
    let current = probe_dist_tag_latest(registry, package, log);
    let guarded = guard_latest_regression(dist_tag, version, current.as_deref());
    if guarded != dist_tag {
        log.status(&format!(
            "npm: publishing {}@{} under inert tag '{}' — registry 'latest' is {} (newer); \
             refusing to regress the default-install pointer",
            package,
            version,
            guarded,
            current.as_deref().unwrap_or("?")
        ));
    }
    guarded
}

/// Resolve the per-package publish credential for one package under the
/// configured [`NpmAuthMode`]: probe existence (only when `auto` needs it),
/// detect OIDC + token availability, run [`decide_auth`], then materialize the
/// actual [`NpmAuth`] (reading the token / OIDC env). Terminal decisions
/// hard-error with a specific, fixable message.
///
/// Returns the chosen [`NpmAuth`] alongside the resolved token string (empty
/// when no token is set) so the caller's OIDC→token fallback need not re-render
/// the token template.
pub(crate) fn resolve_auth_for_package(
    ctx: &Context,
    cfg: &NpmConfig,
    registry: &str,
    package: &str,
    log: &StageLogger,
) -> Result<(NpmAuth, String)> {
    let token = resolve_token(ctx, cfg)?;
    let token_available = !token.is_empty();
    let oidc = resolve_oidc_env(ctx);
    let oidc_available = oidc.is_some();

    // The existence probe only changes the `auto` verdict, and only when at
    // least one credential exists (with neither, the verdict is `ErrorNoAuth`
    // regardless of existence). Skip the network round-trip in the forced
    // `token` / `oidc` modes and when no credential is available.
    let existence = if cfg.auth == NpmAuthMode::Auto && (token_available || oidc_available) {
        probe_package_existence(registry, package, log)
    } else {
        PackageExistence::Unknown
    };

    match decide_auth(cfg.auth, existence, oidc_available, token_available) {
        AuthDecision::Token => Ok((NpmAuth::Token(token.clone()), token)),
        AuthDecision::Oidc => {
            let oidc = oidc.ok_or_else(|| {
                anyhow::anyhow!(
                    "npm: internal — OIDC chosen for '{}' without an OIDC env",
                    package
                )
            })?;
            Ok((NpmAuth::Oidc(oidc), token))
        }
        AuthDecision::FailNewNeedsToken => bail!(
            "npm: package '{}' does not exist and Trusted Publishing cannot create it — \
             set NPM_TOKEN (or cfg.token) for the initial publish, then switch the package \
             to Trusted Publishing once it exists",
            package
        ),
        AuthDecision::ErrorNoAuth => match cfg.auth {
            NpmAuthMode::Token => bail!(
                "npm: auth mode is `token` but no token is set for '{}' — set NPM_TOKEN \
                 (or cfg.token). Refusing to publish anonymously.",
                package
            ),
            NpmAuthMode::Oidc => bail!(
                "npm: auth mode is `oidc` but no OIDC context is present for '{}' — run under \
                 GitHub Actions with `id-token: write` (both ACTIONS_ID_TOKEN_REQUEST_URL and \
                 ACTIONS_ID_TOKEN_REQUEST_TOKEN must be set). Refusing to fall back to a token.",
                package
            ),
            NpmAuthMode::Auto => bail!(
                "npm: cannot authenticate '{}' — set NPM_TOKEN (or cfg.token), or run under \
                 GitHub Actions OIDC (id-token: write) with a Trusted Publisher configured on \
                 the registry. Refusing to publish anonymously.",
                package
            ),
        },
    }
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
