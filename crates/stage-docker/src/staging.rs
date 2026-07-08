use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::target::map_target;

use super::platform::platform_to_arch;

/// Stage artifacts into docker build context using the V2 layout.
///
/// V2 uses `<os>/<arch>/<name>` directory structure (matching `$TARGETPLATFORM`)
/// and stages Binary, LinuxPackage, CArchive, and CShared artifacts.
/// Artifacts with `goos == "all"` are copied into every platform directory.
pub(crate) fn stage_artifacts_v2(
    platforms: &[String],
    staging_dir: &std::path::Path,
    dry_run: bool,
    ids_filter: Option<&Vec<String>>,
    crate_name: &str,
    ctx: &Context,
    log: &StageLogger,
) -> Result<()> {
    let stageable_kinds = [
        ArtifactKind::Binary,
        ArtifactKind::LinuxPackage,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
    ];

    for platform in platforms {
        let parts: Vec<&str> = platform.split('/').collect();
        // Use full platform path (e.g., "linux/amd64") as directory structure
        let platform_dir = staging_dir.join(platform.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !dry_run {
            fs::create_dir_all(&platform_dir).with_context(|| {
                format!("dockers_v2: create platform dir {}", platform_dir.display())
            })?;
        }

        let arch = platform_to_arch(platform);
        let os = parts.first().copied().unwrap_or("linux");

        let mut platform_artifact_count = 0usize;
        for kind in &stageable_kinds {
            let artifacts: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(*kind, crate_name)
                .into_iter()
                .filter(|a| {
                    // Match by architecture, or goos == "all" (cross-platform artifacts)
                    if let Some(target) = a.target.as_deref() {
                        let (a_os, a_arch) = map_target(target);
                        (a_os == os && a_arch == arch) || a_os == "all"
                    } else {
                        // No target = universal artifact, include everywhere
                        true
                    }
                })
                .filter(|a| matches_id_filter(a, ids_filter.map(Vec::as_slice)))
                .collect();

            platform_artifact_count += artifacts.len();
            for artifact in artifacts {
                let file_name = artifact
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact");
                let dest = platform_dir.join(file_name);

                if dry_run {
                    log.status(&format!(
                        "(dry-run) would copy {} → {}",
                        artifact.path.display(),
                        dest.display()
                    ));
                } else {
                    log.status(&format!(
                        "staging {} → {}",
                        artifact.path.display(),
                        dest.display()
                    ));
                    fs::copy(&artifact.path, &dest).with_context(|| {
                        format!(
                            "dockers_v2: copy {} to {}",
                            artifact.path.display(),
                            dest.display()
                        )
                    })?;
                    // fs::copy preserves the source mode, and CI artifact
                    // round-trips strip the exec bit — a plain `COPY` in the
                    // documented Dockerfile pattern would then ship a
                    // non-executable ENTRYPOINT binary. Force 0755 on
                    // executable kinds so the staged context always yields a
                    // runnable image. Packages/archives keep their copied mode.
                    #[cfg(unix)]
                    if matches!(kind, ArtifactKind::Binary | ArtifactKind::CShared) {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
                            .with_context(|| {
                                format!("dockers_v2: chmod 0755 {}", dest.display())
                            })?;
                    }
                }
            }
        }

        if platform_artifact_count == 0 {
            log.skip_line(
                ctx.options.show_skipped,
                &format!(
                    "skipped docker image for platform {} — no binaries (check ids/binary filters)",
                    platform
                ),
            );
        }
    }
    Ok(())
}

/// Copy a Dockerfile into the staging directory.
pub fn copy_dockerfile(
    dockerfile: &str,
    staging_dir: &std::path::Path,
    dry_run: bool,
    log: &StageLogger,
    prefix: &str,
) -> Result<()> {
    let dockerfile_src = PathBuf::from(dockerfile);
    let dockerfile_dest = staging_dir.join("Dockerfile");

    if dry_run {
        log.status(&format!(
            "(dry-run) would copy Dockerfile {} → {}",
            dockerfile_src.display(),
            dockerfile_dest.display()
        ));
    } else {
        log.status(&format!(
            "copying Dockerfile {} → {}",
            dockerfile_src.display(),
            dockerfile_dest.display()
        ));
        fs::copy(&dockerfile_src, &dockerfile_dest).with_context(|| {
            format!(
                "{}: copy Dockerfile from {} to {}",
                prefix,
                dockerfile_src.display(),
                dockerfile_dest.display()
            )
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// warn_project_markers_in_extra_files
// ---------------------------------------------------------------------------

/// Project root markers that likely don't belong in Docker images.
pub const PROJECT_MARKERS: &[&str] = &[
    "go.mod",
    "go.sum",
    "Cargo.toml",
    "Cargo.lock",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "Gemfile",
    "Gemfile.lock",
    "Makefile",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

pub(crate) fn warn_project_markers_in_extra_files(
    extra_files: &[String],
    log: &StageLogger,
    label: &str,
) {
    for file in extra_files {
        let filename = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file);
        if PROJECT_MARKERS.contains(&filename) {
            log.warn(&format!(
                "extra_files for {} contains '{}' which looks like a project root marker — \
                 this likely shouldn't be in a Docker image",
                label, file
            ));
        }
    }
}

/// Copy extra files into the staging directory.
///
/// Preserves relative directory structure for relative paths. For absolute
/// paths, only the filename is used.
///
/// `base_dir` roots relative SOURCE paths: `None` resolves them against the
/// process working directory (the production release path, whose cwd is the
/// repo root); `Some(dir)` resolves them against `dir` (the determinism
/// harness's per-run worktree) so the copy reads the COMMITTED bytes without
/// mutating the process-global cwd. The DEST always preserves the configured
/// relative structure, so both callers stage identical layouts.
pub fn stage_extra_files(
    extra_files: &[String],
    staging_dir: &std::path::Path,
    base_dir: Option<&std::path::Path>,
    dry_run: bool,
    log: &StageLogger,
    prefix: &str,
) -> Result<()> {
    for file_path in extra_files {
        let configured = PathBuf::from(file_path);
        // Resolve the SOURCE: a relative entry roots at `base_dir` when given,
        // else the process cwd; an absolute entry is used verbatim.
        let src = match base_dir {
            Some(base) if configured.is_relative() => base.join(&configured),
            _ => configured.clone(),
        };
        if src.is_dir() {
            anyhow::bail!(
                "{}: extra_files entry '{}' is a directory; only files are supported",
                prefix,
                file_path
            );
        }
        let dest = if configured.is_absolute() {
            let file_name = configured
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(file_path));
            staging_dir.join(file_name)
        } else {
            staging_dir.join(&configured)
        };

        if dry_run {
            log.status(&format!(
                "(dry-run) would copy extra file {} → {}",
                src.display(),
                dest.display()
            ));
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "{}: create parent dirs for extra file {}",
                        prefix,
                        dest.display()
                    )
                })?;
            }
            log.status(&format!(
                "copying extra file {} → {}",
                src.display(),
                dest.display()
            ));
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "{}: copy extra file {} to {}",
                    prefix,
                    src.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}
