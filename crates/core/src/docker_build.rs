//! `docker buildx build` invocation for the determinism harness.
//!
//! Allow-listed entry point for `Command::new("docker")` calls that drive
//! the BuildKit reproducibility side of the determinism harness. The
//! harness itself lives in `crates/cli/` which is forbid-listed for
//! direct subprocess spawn (see `.claude/rules/module-boundaries.md`);
//! this module owns the call site so the security surface stays small.
//!
//! ## What it does
//!
//! `oci_build_fixture` invokes
//!
//! ```text
//! docker buildx build --provenance=false --sbom=false \
//!                     --output=type=oci,rewrite-timestamp=true,dest=<oci_tar> \
//!                     --tag <tag> \
//!                     <context_dir>
//! ```
//!
//! into a hermetic OCI tarball on disk, then returns the SHA-256 of the
//! tarball plus the BuildKit-reported image digest (parsed from the
//! `--iidfile`). The harness fingerprints both and diffs across runs.
//!
//! ## Determinism workarounds applied
//!
//! - **File mtimes inside image layers**: the `rewrite-timestamp=true`
//!   attribute on the `--output type=oci,...` exporter (BuildKit ≥ 0.13)
//!   rewrites every layer entry's mtime to `SOURCE_DATE_EPOCH`. The
//!   harness exports `SOURCE_DATE_EPOCH` via its hermetic env block so
//!   the attribute has a value to rewrite to. The attribute is a
//!   BuildKit *output*-side feature, NOT a top-level `--rewrite-timestamp`
//!   flag (early BuildKit drafts considered the flag form but landed on
//!   the exporter attribute; buildx itself does not surface a top-level
//!   flag).
//! - **Provenance attestation**: `--provenance=false` suppresses BuildKit's
//!   default in-toto provenance attestation, whose body embeds the build
//!   timestamp and BuildKit version — both vary across runs. Operators
//!   who want signed provenance should layer cosign on top after the
//!   harness has proven layer byte-stability.
//! - **SBOM attestation**: `--sbom=false` suppresses the default SBOM
//!   attestation for the same reason as provenance: the syft scanner
//!   embeds its own scan timestamp.
//! - **OCI output (no registry push)**: `--output=type=oci,dest=<file>`
//!   captures the image as a tarball on disk so byte-stability is
//!   verifiable without a running daemon or network reach. (`type=docker`
//!   would require a daemon; `type=registry,push=true` would require a
//!   registry — both unsuitable for hermetic harness use.)
//!
//! ## Cosign timestamp interplay
//!
//! Cosign's default signature flow (`cosign sign <image>`) uploads a
//! transparency-log entry whose body embeds the signing timestamp, so
//! signatures are non-deterministic by design. The harness does NOT
//! invoke cosign on the produced OCI tar — the sign stage owns that path
//! in production. If a future workflow signs the harness-produced image
//! for transparency-log inclusion, the operator must pass
//! `--tlog-upload=false` (and accept the lost transparency property) for
//! byte-stable signatures.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a single `oci_build_fixture` invocation.
#[derive(Debug, Clone)]
pub struct OciBuildOutput {
    /// Absolute path to the emitted OCI tarball on disk.
    pub oci_tar_path: PathBuf,
    /// `sha256:<hex>` digest of the OCI tarball — the harness's
    /// byte-stability fingerprint. Stable across runs iff every layer
    /// (and the manifest index, and every annotation) is reproducible.
    pub oci_tar_sha256: String,
    /// BuildKit-reported image digest from `--iidfile`. Independent of
    /// the tarball hash because the iidfile records the manifest /
    /// manifest-list digest before serialization, while the tarball hash
    /// covers the serialized bytes (which include layer tar member
    /// ordering). Both must be stable across runs for the harness to
    /// declare the image byte-stable.
    pub image_digest: Option<String>,
}

/// Run `docker buildx build` against `context_dir`, producing an OCI
/// tarball at `<context_dir>/.det-out.tar` (path returned in the result).
///
/// `image_tag` is the buildx `--tag` value — used by BuildKit to populate
/// the manifest's `org.opencontainers.image.ref.name` annotation. The
/// harness picks a deterministic constant tag so the annotation does not
/// itself become a drift source.
///
/// `env` carries the harness's hermetic env block (`SOURCE_DATE_EPOCH`,
/// `HOME`, `PATH`, etc.). The function `env_clear`s the child first so
/// host env vars cannot perturb the build.
///
/// Returns `Ok(OciBuildOutput)` on `docker buildx build` exit 0; bubbles
/// a context-wrapped error otherwise.
pub fn oci_build_fixture(
    context_dir: &Path,
    image_tag: &str,
    env: &HashMap<String, String>,
) -> Result<OciBuildOutput> {
    let oci_tar = context_dir.join(".det-out.tar");
    let iidfile = context_dir.join(".det-iidfile.txt");
    // Defensive: a prior aborted invocation can leave stale output behind.
    // `docker buildx build --output=type=oci,dest=<file>` overwrites the
    // file, but the iidfile would silently carry the prior run's digest
    // if buildx fails before writing.
    let _ = std::fs::remove_file(&oci_tar);
    let _ = std::fs::remove_file(&iidfile);

    let mut cmd = Command::new("docker");
    cmd.arg("buildx")
        .arg("build")
        // Suppress provenance + SBOM attestations whose bodies embed
        // wall-clock timestamps and BuildKit version strings.
        .arg("--provenance=false")
        .arg("--sbom=false")
        // BuildKit ≥ 0.13: `rewrite-timestamp=true` on the OCI exporter
        // rewrites every layer entry's mtime to SOURCE_DATE_EPOCH (which
        // the caller exports in `env`). The attribute lives on the
        // `--output` exporter, not as a separate top-level flag — the
        // top-level form was proposed but never landed in buildx.
        .arg(format!(
            "--output=type=oci,rewrite-timestamp=true,dest={}",
            oci_tar.to_string_lossy()
        ))
        .arg(format!("--iidfile={}", iidfile.to_string_lossy()))
        .arg("--tag")
        .arg(image_tag)
        .arg(context_dir);
    cmd.current_dir(context_dir);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    // `docker buildx build` writes a wall-clock-stamped progress stream to
    // stderr by default; suppressing it would silence build errors too.
    // Inherit stdio so failures land in the harness's per-run log.
    let status = cmd.status().with_context(|| {
        format!(
            "spawning `docker buildx build` in {}",
            context_dir.display()
        )
    })?;
    anyhow::ensure!(
        status.success(),
        "`docker buildx build` failed in {} (exit {:?})",
        context_dir.display(),
        status.code()
    );

    anyhow::ensure!(
        oci_tar.exists(),
        "buildx exited 0 but no OCI tarball at {}",
        oci_tar.display()
    );

    let bytes =
        std::fs::read(&oci_tar).with_context(|| format!("reading {}", oci_tar.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let oci_tar_sha256 = format!("sha256:{:x}", hasher.finalize());

    let image_digest = std::fs::read_to_string(&iidfile)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Ok(OciBuildOutput {
        oci_tar_path: oci_tar,
        oci_tar_sha256,
        image_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_build_fails_when_context_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        let env: HashMap<String, String> = HashMap::new();
        let res = oci_build_fixture(&nonexistent, "anodize/det:test", &env);
        assert!(
            res.is_err(),
            "buildx against a nonexistent context dir must error"
        );
    }
}
