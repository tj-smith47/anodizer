//! Source-distribution build, delegated to `maturin sdist`.
//!
//! anodizer never synthesizes a `pyproject.toml` — sdist consumers build
//! from source, so the project must own a real maturin manifest. The
//! publisher shells out to `maturin sdist --manifest-path <dir>/pyproject.toml`
//! and uploads whatever tarball maturin produces.

use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

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
