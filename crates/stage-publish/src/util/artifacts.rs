//! OS / arch inference + artifact lookup helpers used by every package
//! manager publisher (homebrew, scoop, winget, krew, nix, chocolatey).
//!
//! Two-layer normalisation: generic inference from a Rust target triple
//! to canonical short forms (`linux`/`darwin`/`windows`, `amd64`/`arm64`),
//! then publisher-specific mapping at each call-site (e.g. `krew_os`).

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anyhow::Result;

use anodizer_core::artifact::matches_id_filter;

/// Find a Windows Archive artifact and return `(url, sha256)`, or bail with a
/// descriptive error.
#[allow(dead_code)]
pub(crate) fn require_windows_artifact(
    ctx: &Context,
    crate_name: &str,
    label: &str,
) -> Result<(String, String)> {
    find_windows_artifact(ctx, crate_name).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: no Windows archive artifact found for crate '{}'",
            label,
            crate_name
        )
    })
}

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
// Both `find_artifacts_by_os` and `find_all_platform_artifacts` use these
// shared helpers so the inference logic lives in exactly one place.

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
pub(crate) struct OsArtifact {
    pub url: String,
    pub sha256: String,
    pub os: String,
    pub arch: String,
    #[allow(dead_code)]
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
}

/// Convert a single `Artifact` reference into an `OsArtifact`, using the
/// shared `infer_os` / `infer_arch` helpers.
///
/// `os_fallback` is used when the OS cannot be determined from the target
/// triple (e.g. when calling from `find_artifacts_by_os` with a known needle).
fn artifact_to_os_artifact(a: &Artifact, os_fallback: &str) -> OsArtifact {
    let url = a
        .metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| a.path.to_string_lossy().into_owned());
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
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
    OsArtifact {
        url,
        sha256,
        os: infer_os(target, os_fallback),
        arch: infer_arch(target),
        id,
        amd64_variant,
        arm_variant,
        binary,
    }
}

/// Filter a vec of `OsArtifact` by IDs: when `ids` is `Some`, keep only
/// artifacts whose `id` field matches one of the given IDs.  When `ids` is
/// `None`, all artifacts pass through.
#[allow(dead_code)]
pub(crate) fn filter_os_artifacts_by_ids(
    artifacts: Vec<OsArtifact>,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    if let Some(ids) = ids {
        artifacts
            .into_iter()
            .filter(|a| {
                a.id.as_ref()
                    .map(|id| ids.iter().any(|i| i == id))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        artifacts
    }
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

/// Find all Archive artifacts for the given crate whose target or path
/// matches `os_needle` (e.g. "linux", "darwin", "windows").
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
#[allow(dead_code)]
pub(crate) fn find_artifacts_by_os(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
) -> Vec<OsArtifact> {
    find_artifacts_by_os_filtered(ctx, crate_name, os_needle, None)
}

/// Find all Archive artifacts for the given crate whose target or path
/// matches `os_needle`, with optional IDs filter.
pub(crate) fn find_artifacts_by_os_filtered(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    find_artifacts_by_os_with_variant(ctx, crate_name, os_needle, ids, None, None)
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
) -> Vec<OsArtifact> {
    // Include both Archive and UploadableBinary artifacts — GoReleaser
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
    // single-arch variants (GoReleaser parity).
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
        .map(|a| artifact_to_os_artifact(a, os_needle))
        .collect();
    filter_by_variant(os_artifacts, amd64_variant, arm_variant)
}

/// Find all Archive artifacts for the given crate across all platforms.
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
#[allow(dead_code)]
pub(crate) fn find_all_platform_artifacts(ctx: &Context, crate_name: &str) -> Vec<OsArtifact> {
    find_all_platform_artifacts_filtered(ctx, crate_name, None)
}

/// Find all Archive and Binary artifacts for the given crate across all platforms,
/// with optional IDs filter.
pub(crate) fn find_all_platform_artifacts_filtered(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    find_all_platform_artifacts_with_variant(ctx, crate_name, ids, None, None)
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
) -> Vec<OsArtifact> {
    let mut all = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    all.extend(
        ctx.artifacts
            .by_kind_and_crate(ArtifactKind::UploadableBinary, crate_name),
    );
    // OnlyReplacingUnibins: exclude universal binaries that didn't replace
    // single-arch variants (GoReleaser parity).
    let all: Vec<_> = all
        .into_iter()
        .filter(|a| a.only_replacing_unibins())
        .collect();
    let filtered = filter_by_ids(all, ids);
    let os_artifacts: Vec<OsArtifact> = filtered
        .into_iter()
        .map(|a| artifact_to_os_artifact(a, "unknown"))
        .collect();
    filter_by_variant(os_artifacts, amd64_variant, arm_variant)
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

/// Find a Windows Archive artifact for the given crate and return `(url, sha256)`.
///
/// Returns `None` when no matching artifact exists.
#[allow(dead_code)]
pub(crate) fn find_windows_artifact(ctx: &Context, crate_name: &str) -> Option<(String, String)> {
    let a = find_artifacts_by_os(ctx, crate_name, "windows")
        .into_iter()
        .next()?;
    Some((a.url, a.sha256))
}
