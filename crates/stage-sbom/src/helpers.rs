use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;

/// One artifact an SBOM config selected: path, metadata, build target, and
/// kind. The kind is `None` only for the synthetic whole-project subject of
/// `artifacts: any` (which catalogs the source tree, not one artifact).
pub(crate) type SbomSubject = (
    PathBuf,
    HashMap<String, String>,
    Option<String>,
    Option<ArtifactKind>,
);

/// Map a typed (non-`any`, non-`binary`) `artifacts:` filter value to the
/// artifact kind it selects. Shared by both generation modes and the
/// expected-asset derivation so the selection cannot drift between them.
pub(crate) fn typed_artifact_kind(artifacts_type: &str, id: &str) -> Result<ArtifactKind> {
    match artifacts_type {
        "source" => Ok(ArtifactKind::SourceArchive),
        "archive" => Ok(ArtifactKind::Archive),
        "package" => Ok(ArtifactKind::LinuxPackage),
        "diskimage" => Ok(ArtifactKind::DiskImage),
        "installer" => Ok(ArtifactKind::Installer),
        other => bail!(
            "sbom[{}]: unknown artifacts type '{}'. Valid values are: \
             source, archive, package, diskimage, installer, binary, any",
            id,
            other
        ),
    }
}

/// Build the per-artifact template-variable overlay used to render SBOM
/// `documents:` / `args:` / `env:` templates (`ArtifactName`, `ArtifactExt`,
/// `ArtifactID`, and `Os`/`Arch`/`Target` when the artifact has a build
/// target).
///
/// Returns a CLONE of the context's vars with the bindings applied — the
/// shared context is never mutated, so one artifact's `Os`/`Arch`/`Target`
/// cannot leak into the next artifact (or into downstream stages). Shared by
/// both generation modes and the expected-asset derivation so all three
/// render with identical bindings.
pub(crate) fn artifact_template_vars(
    ctx: &Context,
    artifact_path: &Path,
    artifact_meta: &HashMap<String, String>,
    artifact_target: Option<&str>,
) -> anodizer_core::template::TemplateVars {
    let artifact_name = artifact_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let mut vars = ctx.template_vars().clone();
    vars.set("ArtifactName", artifact_name);
    vars.set(
        "ArtifactExt",
        artifact_meta
            .get("ext")
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .unwrap_or_else(|| anodizer_core::template::extract_artifact_ext(artifact_name)),
    );
    vars.set(
        "ArtifactID",
        artifact_meta.get("id").map(|s| s.as_str()).unwrap_or(""),
    );
    let target = artifact_target.or_else(|| artifact_meta.get("target").map(String::as_str));
    if let Some(target) = target {
        let (os, arch) = anodizer_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    }
    vars
}

/// Warn when a configured `ids:` filter is the reason an SBOM config matched
/// nothing — a typo'd build id would otherwise silently no-op the config.
pub(crate) fn warn_ids_eliminated_all(
    log: &anodizer_core::log::StageLogger,
    id: &str,
    ids: Option<&[String]>,
    pre_filter: usize,
    post_filter: usize,
) {
    if anodizer_core::artifact::ids_filter_eliminated_all(ids, pre_filter, post_filter) {
        log.warn(&format!(
            "ids filter {:?} on sbom[{}] matched no artifacts — this config will \
             produce NO SBOMs",
            ids.unwrap_or(&[]),
            id
        ));
    }
}

/// Detect the built-in SBOM format (and its file extension) from the
/// `documents:` templates' trailing extension chain.
/// `mytool-spdx-companion.cdx.json` resolves to CycloneDX because the
/// trailing extension is `.cdx.json`; a raw substring match on the marketing
/// word in the basename would flip to SPDX and produce a
/// CycloneDX-by-name / SPDX-by-payload file.
pub(crate) fn builtin_format_and_extension(documents: &[String]) -> (&'static str, &'static str) {
    let mut detected = ("cyclonedx", "cdx.json");
    for d in documents {
        let lower = d.to_lowercase();
        if lower.ends_with(".spdx.json") || lower.ends_with(".spdx") {
            detected = ("spdx", "spdx.json");
            break;
        }
        if lower.ends_with(".cdx.json") || lower.ends_with(".cyclonedx.json") {
            detected = ("cyclonedx", "cdx.json");
            break;
        }
    }
    detected
}
