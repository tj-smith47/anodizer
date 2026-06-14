//! OS / arch inference + artifact lookup helpers used by every package
//! manager publisher (homebrew, scoop, winget, krew, nix, chocolatey).
//!
//! Two-layer normalisation: generic inference from a Rust target triple
//! to canonical short forms (`linux`/`darwin`/`windows`, `amd64`/`arm64`),
//! then publisher-specific mapping at each call-site (e.g. `krew_os`).

use anodizer_core::artifact::matches_id_filter;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anyhow::{Result, bail};

// ---------------------------------------------------------------------------
// OS / architecture inference from target triples
// ---------------------------------------------------------------------------
//
// The functions below provide a two-layer normalisation scheme:
//
// 1. **Generic inference** (`infer_os` / `infer_arch`):
//    Map a Rust-style target triple (e.g. `x86_64-unknown-linux-gnu`,
//    `aarch64-apple-darwin`) to a canonical short form used internally
//    by `OsArtifact` (`"linux"`, `"darwin"`, `"windows"`, `"amd64"`,
//    `"arm64"`).
//
// 2. **Publisher-specific mapping** (e.g. `krew_os`, `krew_arch` in krew.rs):
//    Translate the canonical form to whatever the target ecosystem expects.
//    For Krew the mapping is effectively a no-op today, but keeping a
//    separate layer means we can adjust for future drift without touching
//    the shared inference code.
//
// Both artifact-search functions use these shared helpers so the inference
// logic lives in exactly one place.

/// Infer the canonical OS string from a target triple.
///
/// Delegates to [`anodizer_core::target::map_target`] for the actual parsing.
/// Returns the mapped OS, or `fallback` when the OS is `"unknown"`.
pub(crate) fn infer_os(target: &str, fallback: &str) -> String {
    let (os, _) = anodizer_core::target::map_target(target);
    if os == "unknown" {
        fallback.to_string()
    } else {
        os
    }
}

/// Infer the canonical architecture string from a target triple.
///
/// Delegates to [`anodizer_core::target::map_target`] for the actual parsing.
pub(crate) fn infer_arch(target: &str) -> String {
    let (_, arch) = anodizer_core::target::map_target(target);
    arch
}

/// Describes the OS + architecture of an artifact match.
#[derive(Debug, Default)]
pub(crate) struct OsArtifact {
    pub url: String,
    pub sha256: String,
    pub os: String,
    pub arch: String,
    /// Artifact ID from metadata (matches the `id:` field in archive configs).
    /// Used by publishers such as `nix` to correlate artifacts to their
    /// per-archive configuration entries.
    pub id: Option<String>,
    /// amd64 microarchitecture variant (e.g. "v1", "v2", "v3", "v4").
    /// Populated from artifact metadata when present.
    pub amd64_variant: Option<String>,
    /// ARM version (e.g. "6", "7").
    /// Populated from artifact metadata when present.
    pub arm_variant: Option<String>,
    /// In-archive binary filename for archive artifacts (first entry of
    /// `extra_binaries`), or the binary name for `UploadableBinary`
    /// artifacts. None when neither is present. Krew/Scoop/etc. publishers
    /// rely on this to point `bin:` at the actual file inside the archive,
    /// which can differ from the crate name (e.g. crate `my-tool` ships
    /// binary `mytool`) and ends in `.exe` on Windows.
    pub binary: Option<String>,
    /// In-archive `wrap_in_directory` prefix, when the archive wraps its
    /// contents in a top-level directory (`metadata["wrap_in_directory"]`).
    /// `None` for a flat archive. Krew's `files[].from` must reference the
    /// nested path (`<prefix>/<bin>`) for `bin:` to resolve, so the krew
    /// publisher reads this to shape its extraction list correctly.
    pub wrap_in_directory: Option<String>,
    /// Bundled non-binary in-archive paths (LICENSE / README / completions /
    /// man), from `metadata["archive_files"]`. Already carries the
    /// `wrap_in_directory` prefix. Empty when the archive bundles no extra
    /// files. The krew publisher selects LICENSE/README entries from this so
    /// its `files:` list is gated on actual presence rather than guessed.
    pub archive_files: Vec<String>,
}

/// Convert a single `Artifact` reference into an `OsArtifact`, using the
/// shared `infer_os` / `infer_arch` helpers.
///
/// `os_fallback` is used when the OS cannot be determined from the target
/// triple (e.g. when calling with a known OS needle).
///
/// `require_url` controls behaviour when `metadata["url"]` is absent:
/// - `true` (production publish): errors loudly — a publisher that proceeds
///   without a URL would embed a local path in a manifest it then submits.
/// - `false` (snapshot / dry-run validation): falls back to `a.name` (just the
///   archive filename, not the local `./dist/…` path). Snapshot cross-checks
///   compare asset filenames, not full URLs, so the placeholder is correct there
///   and is obviously not a real download URL if anything inspects it.
fn artifact_to_os_artifact(
    a: &Artifact,
    os_fallback: &str,
    require_url: bool,
) -> Result<OsArtifact> {
    let url = match a.metadata.get("url").cloned() {
        Some(u) => u,
        None if !require_url => a.name.clone(),
        None => {
            return Err(anyhow::anyhow!(
                "artifact '{}' (target={}) has no download URL in metadata \
                 — ensure the release stage ran and uploaded artifacts before \
                 the publish stage, or pass --dist pointing to a dist with \
                 uploaded artifacts",
                a.path.display(),
                a.target.as_deref().unwrap_or("<none>"),
            ));
        }
    };
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    if sha256.is_empty() {
        bail!(
            "artifact '{}' (target={}) has missing sha256 metadata \
             — ensure the checksum stage runs before publish",
            a.path.display(),
            a.target.as_deref().unwrap_or("<none>"),
        );
    }
    let id = a.metadata.get("id").cloned();
    let amd64_variant = a.metadata.get("amd64_variant").cloned();
    let arm_variant = a.metadata.get("arm_variant").cloned();
    let target = a.target.as_deref().unwrap_or("");
    // Prefer archive's first extra_binaries entry; fall back to the artifact's
    // own `binary` metadata (set on UploadableBinary). None when this artifact
    // has no associated binary name (caller may substitute crate_name).
    let binary = a
        .extra_binaries()
        .into_iter()
        .next()
        .or_else(|| a.metadata.get("binary").cloned());
    let wrap_in_directory = a
        .metadata
        .get("wrap_in_directory")
        .cloned()
        .filter(|s| !s.is_empty());
    let archive_files = a
        .metadata
        .get("archive_files")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(OsArtifact {
        url,
        sha256,
        os: infer_os(target, os_fallback),
        arch: infer_arch(target),
        id,
        amd64_variant,
        arm_variant,
        binary,
        wrap_in_directory,
        archive_files,
    })
}

/// Filter artifacts by IDs using the canonical `matches_id_filter` semantics.
pub(crate) fn filter_by_ids<'a>(
    artifacts: Vec<&'a Artifact>,
    ids: Option<&[String]>,
) -> Vec<&'a Artifact> {
    artifacts
        .into_iter()
        .filter(|a| matches_id_filter(a, ids))
        .collect()
}

/// Find artifacts by OS with optional amd64_variant/arm_variant microarchitecture filtering.
///
/// When `amd64_variant` is `Some`, only amd64 artifacts whose metadata `amd64_variant`
/// matches (or have no amd64_variant metadata) are included.
/// Similarly for `arm_variant` and arm artifacts.
pub(crate) fn find_artifacts_by_os_with_variant(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
    ids: Option<&[String]>,
    amd64_variant: Option<&str>,
    arm_variant: Option<&str>,
) -> Result<Vec<OsArtifact>> {
    let require_url = !ctx.is_snapshot() && !ctx.is_dry_run();
    // Include both Archive and UploadableBinary artifacts — both
    // supports both UploadableArchive and UploadableBinary types for publisher
    // packages. Use UploadableBinary (not Binary) so raw build outputs
    // packaged into archives don't double-register as portable binaries.
    let mut all = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    all.extend(
        ctx.artifacts
            .by_kind_and_crate(ArtifactKind::UploadableBinary, crate_name),
    );
    // OnlyReplacingUnibins: exclude universal binaries that didn't replace
    // single-arch variants.
    let all: Vec<_> = all
        .into_iter()
        .filter(|a| a.only_replacing_unibins())
        .collect();
    let filtered = filter_by_ids(all, ids);
    let os_artifacts: Vec<OsArtifact> = filtered
        .into_iter()
        .filter(|a| {
            a.target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains(os_needle))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(os_needle)
        })
        .map(|a| artifact_to_os_artifact(a, os_needle, require_url))
        .collect::<Result<Vec<_>>>()?;
    Ok(filter_by_variant(os_artifacts, amd64_variant, arm_variant))
}

/// Find all platform artifacts with optional amd64_variant/arm_variant microarchitecture
/// filtering.
///
/// When `amd64_variant` is `Some`, only amd64 artifacts whose metadata `amd64_variant`
/// matches (or have no amd64_variant metadata) are included.
/// Similarly for `arm_variant` and arm artifacts.
pub(crate) fn find_all_platform_artifacts_with_variant(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
    amd64_variant: Option<&str>,
    arm_variant: Option<&str>,
) -> Result<Vec<OsArtifact>> {
    let require_url = !ctx.is_snapshot() && !ctx.is_dry_run();
    let mut all = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    all.extend(
        ctx.artifacts
            .by_kind_and_crate(ArtifactKind::UploadableBinary, crate_name),
    );
    // OnlyReplacingUnibins: exclude universal binaries that didn't replace
    // single-arch variants.
    let all: Vec<_> = all
        .into_iter()
        .filter(|a| a.only_replacing_unibins())
        .collect();
    let filtered = filter_by_ids(all, ids);
    let os_artifacts: Vec<OsArtifact> = filtered
        .into_iter()
        .map(|a| artifact_to_os_artifact(a, "unknown", require_url))
        .collect::<Result<Vec<_>>>()?;
    Ok(filter_by_variant(os_artifacts, amd64_variant, arm_variant))
}

/// Filter a vec of `OsArtifact` by amd64_variant/arm_variant microarchitecture variants.
///
/// For amd64 artifacts: when `amd64_variant` is set, keep only artifacts whose
/// `amd64_variant` metadata matches the config value or that have no amd64_variant
/// metadata.
///
/// For arm artifacts (armv6, armv7): when `arm_variant` is set, keep only artifacts
/// whose `arm_variant` metadata matches or that have no arm_variant metadata.
///
/// Non-amd64/non-arm artifacts always pass through.
pub(super) fn filter_by_variant(
    artifacts: Vec<OsArtifact>,
    amd64_variant: Option<&str>,
    arm_variant: Option<&str>,
) -> Vec<OsArtifact> {
    artifacts
        .into_iter()
        .filter(|a| {
            // Filter amd64 artifacts by amd64_variant config
            if a.arch == "amd64"
                && let Some(want) = amd64_variant
            {
                // Keep if artifact has no amd64_variant (compat) or matches
                return a.amd64_variant.as_deref().is_none_or(|v| v == want);
            }
            // Filter arm artifacts by arm_variant config
            if a.arch.starts_with("arm")
                && a.arch != "arm64"
                && let Some(want) = arm_variant
            {
                return a.arm_variant.as_deref().is_none_or(|v| v == want);
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn bare_archive(target: &str) -> Artifact {
        Artifact {
            path: PathBuf::from(format!("dist/tool-1.0.0-{target}.tar.gz")),
            name: format!("tool-1.0.0-{target}.tar.gz"),
            crate_name: "tool".to_string(),
            kind: ArtifactKind::Archive,
            target: Some(target.to_string()),
            metadata: HashMap::new(),
            size: None,
        }
    }

    fn archive_with_url(target: &str, url: &str, sha256: &str) -> Artifact {
        let mut a = bare_archive(target);
        a.metadata.insert("url".to_string(), url.to_string());
        a.metadata.insert("sha256".to_string(), sha256.to_string());
        a
    }

    /// Missing `metadata["url"]` must produce a descriptive error rather than
    /// silently using the local path (which produces broken PKGBUILD source
    /// entries that AUR / homebrew / etc. reject at submission time).
    #[test]
    fn artifact_to_os_artifact_errors_when_url_absent() {
        let mut a = bare_archive("x86_64-unknown-linux-gnu");
        a.metadata
            .insert("sha256".to_string(), "abc123".to_string());
        let err = artifact_to_os_artifact(&a, "linux", true).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no download URL"),
            "error must mention missing URL: {msg}"
        );
        assert!(
            msg.contains("release stage"),
            "error must point at the release stage: {msg}"
        );
    }

    /// Both `metadata["url"]` and `metadata["sha256"]` present — conversion
    /// succeeds and the URL field carries the download URL.
    #[test]
    fn artifact_to_os_artifact_succeeds_with_url_and_sha256() {
        let a = archive_with_url(
            "x86_64-unknown-linux-gnu",
            "https://example.com/tool-1.0.0-linux-amd64.tar.gz",
            "deadbeef",
        );
        let osa = artifact_to_os_artifact(&a, "linux", true).expect("must succeed");
        assert_eq!(osa.url, "https://example.com/tool-1.0.0-linux-amd64.tar.gz");
        assert_eq!(osa.sha256, "deadbeef");
        assert_eq!(osa.arch, "amd64");
        assert_eq!(osa.os, "linux");
    }

    /// In snapshot / dry-run mode (`require_url = false`) a missing URL falls
    /// back to the artifact filename (not the full local `./dist/…` path).
    /// Snapshot cross-checks compare asset filenames, so the placeholder is
    /// correct there and is obviously not a real URL if anything else inspects it.
    #[test]
    fn artifact_to_os_artifact_lenient_uses_name_when_url_absent() {
        let mut a = bare_archive("x86_64-unknown-linux-gnu");
        a.metadata
            .insert("sha256".to_string(), "abc123".to_string());
        let osa = artifact_to_os_artifact(&a, "linux", false).expect("lenient must succeed");
        assert_eq!(
            osa.url, "tool-1.0.0-x86_64-unknown-linux-gnu.tar.gz",
            "placeholder URL must be the artifact filename, not the local path"
        );
        assert_eq!(osa.sha256, "abc123");
    }
}
