//! NPM tarball staging: assemble per-mode `package/` directories, copy and
//! render extra files, and pack deterministic `.tgz` archives.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anodizer_core::config::{NpmConfig, NpmTemplatedExtraFile};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template_file_render::render_templated_file_entry;
use anyhow::{Context as _, Result};
use tempfile::TempDir;

use super::manifest::{
    PlatformBinary, render_launcher_js, render_package_json, render_postinstall_js,
    resolve_extra_files, resolve_name,
};

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

/// Write `bytes` to `path` with a deterministic mode (`.js` → `0o755`, else
/// `0o644`) so the resulting `.tgz` is byte-identical across runs.
pub(super) fn write_deterministic(path: &Path, bytes: &[u8]) -> Result<()> {
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
