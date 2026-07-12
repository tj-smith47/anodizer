//! Source-distribution build, delegated to `maturin sdist`.
//!
//! anodizer never synthesizes a `pyproject.toml` — sdist consumers build
//! from source, so the project must own a real maturin manifest. The
//! publisher shells out to `maturin sdist --manifest-path <dir>/pyproject.toml`
//! and uploads whatever tarball maturin produces.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

/// The `PKG-INFO` fields the legacy upload form must echo for an sdist.
///
/// Warehouse validates the upload form's `metadata_version` / `name` /
/// `version` against the values embedded in the tarball's own `PKG-INFO`, so
/// they are parsed FROM the maturin-built sdist rather than assumed — maturin
/// emits its own `Metadata-Version` (2.3/2.4) and the `pyproject.toml`
/// version, which need not match anodizer's wheel METADATA (2.1) or the cargo
/// version form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SdistPkgInfo {
    pub metadata_version: String,
    pub name: String,
    pub version: String,
}

/// Parse the `metadata_version` / `name` / `version` headers from an sdist
/// tarball's top-level `PKG-INFO` (`<name>-<version>/PKG-INFO`). `PKG-INFO`
/// is an RFC 822-style header block; only the three fields the upload form
/// needs are extracted, from the first `PKG-INFO` entry found.
pub(crate) fn parse_pkg_info(sdist_path: &Path) -> Result<SdistPkgInfo> {
    let file = std::fs::File::open(sdist_path)
        .with_context(|| format!("pypi: open sdist '{}'", sdist_path.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut body = None;
    for entry in archive.entries().context("pypi: read sdist tar entries")? {
        let mut entry = entry.context("pypi: read sdist tar entry")?;
        // Top-level `<name>-<version>/PKG-INFO` — exactly one path component
        // before the file, so a nested `foo.egg-info/PKG-INFO` is ignored.
        // Scoped so the `path` borrow drops before the mutable read below.
        let is_top_level_pkg_info = {
            let path = entry.path().context("pypi: sdist entry path")?;
            path.file_name().is_some_and(|f| f == "PKG-INFO") && path.components().count() == 2
        };
        if is_top_level_pkg_info {
            let mut s = String::new();
            entry
                .read_to_string(&mut s)
                .context("pypi: read PKG-INFO")?;
            body = Some(s);
            break;
        }
    }
    let body = body.ok_or_else(|| {
        anyhow::anyhow!(
            "pypi: sdist '{}' contains no top-level PKG-INFO",
            sdist_path.display()
        )
    })?;
    parse_pkg_info_headers(&body).with_context(|| {
        format!(
            "pypi: sdist '{}' PKG-INFO is missing a required header",
            sdist_path.display()
        )
    })
}

/// Extract the three required headers from a `PKG-INFO` header block. Header
/// parsing stops at the first blank line (the long-description body follows).
fn parse_pkg_info_headers(body: &str) -> Result<SdistPkgInfo> {
    let mut metadata_version = None;
    let mut name = None;
    let mut version = None;
    for line in body.lines() {
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "metadata-version" if metadata_version.is_none() => metadata_version = Some(value),
            "name" if name.is_none() => name = Some(value),
            "version" if version.is_none() => version = Some(value),
            _ => {}
        }
    }
    match (metadata_version, name, version) {
        (Some(metadata_version), Some(name), Some(version)) => Ok(SdistPkgInfo {
            metadata_version,
            name,
            version,
        }),
        _ => bail!("PKG-INFO must carry Metadata-Version, Name, and Version headers"),
    }
}

/// Build the sdist into `out_dir` and return the produced tarball path.
///
/// `manifest_dir` is the (already template-rendered) directory containing
/// `pyproject.toml`. `out_dir` must be a fresh/dedicated directory: the
/// produced file is located as the single `*.tar.gz` in it after the run.
pub(crate) fn build_sdist(
    ctx: &Context,
    manifest_dir: &str,
    out_dir: &Path,
    log: &StageLogger,
) -> Result<PathBuf> {
    let manifest_path = Path::new(manifest_dir).join("pyproject.toml");
    if !manifest_path.exists() {
        bail!(
            "pypi: sdist requested but '{}' does not exist — point `sdist_manifest` \
             at the directory containing your pyproject.toml",
            manifest_path.display()
        );
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("pypi: create sdist staging dir {}", out_dir.display()))?;

    let mut cmd = Command::new("maturin");
    cmd.arg("sdist")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--out")
        .arg(out_dir);
    // Pin SOURCE_DATE_EPOCH from the run context so maturin's tarball
    // timestamps are reproducible across re-runs of the same commit.
    if let Some(epoch) = ctx
        .env_var("SOURCE_DATE_EPOCH")
        .or_else(|| ctx.template_vars().get("CommitTimestamp").cloned())
    {
        cmd.env("SOURCE_DATE_EPOCH", epoch);
    }
    anodizer_core::run::run_checked(&mut cmd, log, "maturin sdist")
        .context("pypi: run `maturin sdist`")?;

    // maturin names the tarball itself ({escaped}-{pep440}.tar.gz); the
    // staging dir is per-entry and starts empty, so the single tarball in it
    // is the one this invocation produced.
    let mut tarballs: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .with_context(|| format!("pypi: list sdist staging dir {}", out_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".tar.gz"))
        .collect();
    match tarballs.len() {
        0 => bail!(
            "pypi: `maturin sdist` reported success but produced no .tar.gz in {}",
            out_dir.display()
        ),
        1 => Ok(tarballs.remove(0)),
        n => bail!(
            "pypi: expected exactly one sdist tarball in {}, found {} — is the \
             staging dir shared between entries?",
            out_dir.display(),
            n
        ),
    }
}
