//! Which artifacts one upload entry selects.
//!
//! Shared by the Artifactory publisher and the generic `uploads:` publisher —
//! both resolve `mode:` to a set of artifact kinds, then narrow that set with
//! the entry's `ids` / `exclude` / `exts` filters and its resolved
//! `extra_files`.

use super::*;

// ---------------------------------------------------------------------------
// validate_upload_mode
// ---------------------------------------------------------------------------

/// Validate the upload mode string. Only `"archive"` and `"binary"` are
/// accepted; matching is case-insensitive so `mode: Archive` works.
///
/// `publisher` prefixes the error so the same shared validator serves both the
/// Artifactory and the generic `uploads:` publisher with a correct label.
pub fn validate_upload_mode_for(publisher: &str, mode: &str) -> Result<()> {
    match mode.to_ascii_lowercase().as_str() {
        "archive" | "binary" => Ok(()),
        _ => Err(anodizer_core::pipe_skip::entry_skip(format!(
            "{}: invalid upload mode '{}' (expected 'archive' or 'binary')",
            publisher, mode
        ))),
    }
}

/// Validate the upload mode for the Artifactory publisher (label `artifactory`).
pub fn validate_upload_mode(mode: &str) -> Result<()> {
    validate_upload_mode_for("artifactory", mode)
}

// ---------------------------------------------------------------------------
// Artifact filtering by mode
// ---------------------------------------------------------------------------

/// Return the artifact kinds that match the given upload mode.
/// `binary` selects compiled binaries; everything else selects every
/// uploadable artifact kind.
pub(crate) fn artifact_kinds_for_mode(mode: &str) -> Vec<ArtifactKind> {
    match mode.to_ascii_lowercase().as_str() {
        "binary" => vec![ArtifactKind::UploadableBinary],
        _ => vec![
            ArtifactKind::Archive,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Flatpak,
            ArtifactKind::SourceRpm,
            ArtifactKind::Sbom,
            ArtifactKind::Snap,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
        ],
    }
}

/// Bundling flags for [`collect_upload_artifacts`].
///
/// Each `bool` toggles inclusion of an extra artifact category alongside
/// the mode-selected primary artifacts. `extra_files_only` short-circuits
/// the entire selection — when set, only [`ArtifactKind::UploadableFile`]
/// items are returned and the other flags are ignored.
#[derive(Clone, Copy, Default)]
pub(crate) struct CollectFlags {
    pub(crate) checksum: bool,
    pub(crate) signature: bool,
    pub(crate) meta: bool,
    pub(crate) extra_files_only: bool,
}

/// Collect artifacts matching mode, optional ID filter, optional extension
/// filter, and optional `exclude:` glob filter.
/// Also collects checksum/signature/metadata artifacts and extra files when configured.
pub(crate) fn collect_upload_artifacts<'a>(
    ctx: &'a Context,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
) -> Vec<&'a Artifact> {
    let CollectFlags {
        checksum: include_checksum,
        signature: include_signature,
        meta: include_meta,
        extra_files_only,
    } = flags;
    // If extra_files_only, skip normal artifacts entirely
    if extra_files_only {
        return ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::UploadableFile)
            .collect();
    }
    let kinds = artifact_kinds_for_mode(mode);
    let mut artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            // Must match one of the mode kinds
            if !kinds.contains(&a.kind) {
                return false;
            }
            // A macOS `.app` bundle is a DIRECTORY; uploading it as a file dies
            // with "the asset to upload can't be a directory". Its wrapping
            // `.dmg`/`.pkg` (both files) are the correct upload subjects.
            if anodizer_core::artifact::is_directory_bundle_artifact(a) {
                return false;
            }
            // ID filter
            if !crate::util::matches_id_filter(a, ids) {
                return false;
            }
            // `exclude:` glob filter — drop sidecars (checksums/sigs/SBOMs)
            // the operator keeps off THIS Artifactory target.
            if !anodizer_core::artifact::passes_exclude_filter(a, exclude) {
                return false;
            }
            // Extension filter (case-folding via the shared matcher).
            if let Some(ext_list) = exts
                && !ext_list.is_empty()
                && !crate::util::format_matches(a.name(), ext_list)
            {
                return false;
            }
            true
        })
        .collect();

    // Optionally include checksum artifacts
    if include_checksum {
        for a in ctx.artifacts.all() {
            if a.kind == ArtifactKind::Checksum {
                artifacts.push(a);
            }
        }
    }
    // Optionally include signature and certificate artifacts
    // Certificate is included alongside Signature.
    if include_signature {
        for a in ctx.artifacts.all() {
            if (a.kind == ArtifactKind::Signature || a.kind == ArtifactKind::Certificate)
                && !anodizer_core::artifact::is_binary_sign_output(a)
            {
                artifacts.push(a);
            }
        }
    }
    // Optionally include metadata artifacts
    if include_meta {
        for a in ctx.artifacts.all() {
            if a.kind == ArtifactKind::Metadata {
                artifacts.push(a);
            }
        }
    }

    artifacts
}

/// Resolve an upload entry's `extra_files` specs into synthetic
/// [`ArtifactKind::UploadableFile`] artifacts, mirroring GoReleaser's
/// `extrafiles.Find` (glob expansion, optional per-file `name_template`
/// override, directory filtering, path de-duplication).
///
/// `publisher` labels any error so the same shared resolver serves both the
/// Artifactory and the generic `uploads:` publisher with a correct prefix.
/// The `name_template` (when set) is rendered through the context's template
/// vars so a user can write `name_template: "{{ .ProjectName }}-extra.txt"`;
/// when unset, the file's base name is used (GoReleaser's default). The
/// resulting artifacts carry no build target, so the per-artifact URL renders
/// without `Os`/`Arch`/`Target` bindings — matching how a non-build asset
/// uploads.
pub(crate) fn resolve_extra_file_artifacts(
    ctx: &Context,
    publisher: &str,
    specs: &[anodizer_core::config::ExtraFileSpec],
    log: &StageLogger,
) -> Result<Vec<Artifact>> {
    let resolved = anodizer_core::extrafiles::resolve(specs, log)
        .with_context(|| format!("{publisher}: resolve extra_files"))?;
    let mut out = Vec::with_capacity(resolved.len());
    for r in resolved {
        // Render the optional name override; fall back to the file's base name
        // (GoReleaser's default when no name_template is set).
        let name = match r.name_template.as_deref() {
            Some(tmpl) => ctx.render_template(tmpl).with_context(|| {
                format!("{publisher}: render extra_files name_template '{tmpl}'")
            })?,
            None => r
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        out.push(Artifact {
            kind: ArtifactKind::UploadableFile,
            path: r.path,
            name,
            target: None,
            crate_name: ctx.config.project_name.clone(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }
    Ok(out)
}

/// Collect an upload entry's full owned artifact set: the mode/ids/exts-filtered
/// release artifacts (plus checksum/signature/meta sidecars) **and** the
/// entry's `extra_files` specs resolved into uploadable artifacts.
///
/// This is the GoReleaser-parity entry point both HTTP-upload publishers drive
/// (`uploadWithFilter` in GoReleaser appends `extrafiles.Find` results to the
/// filtered set on every run). Resolved `extra_files` artifacts are ALWAYS
/// included — with `extra_files_only: true` they are returned alongside any
/// pre-registered [`ArtifactKind::UploadableFile`] artifacts (e.g. from the
/// `template_files:` stage) and the mode-filtered release set is skipped;
/// otherwise they are appended to it. De-duplicates by on-disk path so a file
/// matched by both a glob and a pre-registered artifact uploads once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_upload_artifacts_owned(
    ctx: &Context,
    publisher: &str,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
    extra_files: Option<&[anodizer_core::config::ExtraFileSpec]>,
    log: &StageLogger,
) -> Result<Vec<Artifact>> {
    let mut out: Vec<Artifact> = collect_upload_artifacts(ctx, mode, ids, exclude, exts, flags)
        .into_iter()
        .cloned()
        .collect();

    if let Some(specs) = extra_files
        && !specs.is_empty()
    {
        let resolved = resolve_extra_file_artifacts(ctx, publisher, specs, log)?;
        let mut seen: std::collections::HashSet<std::path::PathBuf> =
            out.iter().map(|a| a.path.clone()).collect();
        for a in resolved {
            if seen.insert(a.path.clone()) {
                out.push(a);
            }
        }
    }

    Ok(out)
}

/// Collect an upload entry's owned artifact set for **rollback-target
/// enumeration**, degrading to the mode/ids/exts-filtered set when
/// `extra_files` resolution fails.
///
/// Used by both publishers' `collect_*_targets` evidence walkers. A quiet
/// logger swallows the `extra_files` glob warnings (rollback enumeration is
/// not a user-facing render pass), and a resolution error only narrows the
/// rollback checklist — the publish path itself called
/// [`collect_upload_artifacts_owned`] with `?`, so any genuine blocker has
/// already surfaced there. The fallback therefore never hides a publish
/// failure; it just keeps the rollback DELETE list as complete as the
/// resolvable inputs allow.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_target_artifacts_best_effort(
    ctx: &Context,
    publisher: &'static str,
    mode: &str,
    ids: Option<&[String]>,
    exclude: Option<&[String]>,
    exts: Option<&[String]>,
    flags: CollectFlags,
    extra_files: Option<&[anodizer_core::config::ExtraFileSpec]>,
) -> Vec<Artifact> {
    let quiet = StageLogger::new(publisher, anodizer_core::log::Verbosity::Quiet);
    collect_upload_artifacts_owned(
        ctx,
        publisher,
        mode,
        ids,
        exclude,
        exts,
        flags,
        extra_files,
        &quiet,
    )
    .unwrap_or_else(|_| {
        collect_upload_artifacts(ctx, mode, ids, exclude, exts, flags)
            .into_iter()
            .cloned()
            .collect()
    })
}
