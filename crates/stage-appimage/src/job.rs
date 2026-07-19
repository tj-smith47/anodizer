use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind};

use super::appdir::{AppDirEntry, assemble_appdir, pin_appdir_mtimes};

// ---------------------------------------------------------------------------
// linuxdeploy command construction (pure — independently testable)
// ---------------------------------------------------------------------------

/// Build the linuxdeploy argument vector for one AppDir.
///
/// `linuxdeploy --appdir <AppDir> -d <desktop> --output appimage`, with any
/// user `extra_args` appended last.
///
/// The icon is deliberately NOT passed via `-i`: [`assemble_appdir`] pre-places
/// it into the AppDir's icon-theme tree under the exact `Icon=` name from the
/// desktop file, and linuxdeploy resolves it from there. Passing `-i` instead
/// would (a) deploy under the icon FILE's basename, which need not match the
/// desktop `Icon=` key, and (b) hard-reject any icon whose pixel resolution is
/// outside linuxdeploy's fixed accepted set (max 512x512) — a perfectly
/// ordinary 1024x1024 source icon would abort the build. An icon already
/// present in the theme tree is not resolution-checked, so pre-placing accepts
/// any size verbatim.
pub(crate) fn linuxdeploy_args(
    appdir: &Path,
    desktop: &Path,
    extra_args: &[String],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--appdir".into(),
        appdir.as_os_str().to_os_string(),
        "-d".into(),
        desktop.as_os_str().to_os_string(),
        "--output".into(),
        "appimage".into(),
    ];
    for a in extra_args {
        args.push(a.into());
    }
    args
}

/// Build the env map linuxdeploy reads for one AppImage.
///
/// `VERSION` / `ARCH` / `APP` are always set, plus the output filename under
/// BOTH `OUTPUT` (legacy) and `LDAI_OUTPUT` (the appimage plugin's current
/// name for it) so the plugin writes the AppImage directly under `out_filename`
/// in linuxdeploy's cwd. The plugin is selected by the `--output appimage` CLI
/// arg (see [`linuxdeploy_args`]); `OUTPUT` is the output FILENAME, not a plugin
/// selector — setting it to `appimage` makes the plugin emit a file literally
/// named `appimage` with no `.AppImage` extension. `UPDATE_INFORMATION` is set
/// only when configured (absent otherwise, so the AppImage carries no zsync
/// metadata — matching linuxdeploy's default).
pub(crate) fn linuxdeploy_env(
    version: &str,
    arch: &str,
    app: &str,
    out_filename: &str,
    update_information: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("VERSION".to_string(), version.to_string()),
        ("ARCH".to_string(), arch.to_string()),
        ("APP".to_string(), app.to_string()),
        ("OUTPUT".to_string(), out_filename.to_string()),
        ("LDAI_OUTPUT".to_string(), out_filename.to_string()),
    ];
    if let Some(ui) = update_information {
        env.push(("UPDATE_INFORMATION".to_string(), ui.to_string()));
    }
    env
}

// ---------------------------------------------------------------------------
// AppImageJob — fully-owned, ready for parallel execution
// ---------------------------------------------------------------------------

/// One fully-prepared AppImage job (one per matching target). The serial
/// phase renders templates + assembles the static parts; the parallel phase
/// builds the AppDir and spawns linuxdeploy. Carries only owned data so
/// worker threads never touch `Context`.
pub(crate) struct AppImageJob {
    pub(crate) id: String,
    pub(crate) filename: String,
    /// Display name passed to linuxdeploy via `APP` and used as AppDir base.
    pub(crate) app_name: String,
    pub(crate) version: String,
    pub(crate) arch_token: String,
    pub(crate) update_information: Option<String>,
    pub(crate) extra_args: Vec<String>,
    pub(crate) appdir_root: PathBuf,
    pub(crate) output_path: PathBuf,
    pub(crate) binary_src: PathBuf,
    pub(crate) binary_name: String,
    pub(crate) desktop_src: PathBuf,
    pub(crate) icon_src: PathBuf,
    /// Extra files/dirs (including the harvested runtime tree) to drop into
    /// the AppDir before linuxdeploy runs.
    pub(crate) appdir_entries: Vec<AppDirEntry>,
    pub(crate) primary_target: Option<String>,
    pub(crate) primary_crate_name: String,
    /// The binary's amd64 micro-architecture variant (`None` / `Some("v1")`
    /// → baseline), recorded in the produced artifact's metadata so downstream
    /// stages can tell two amd64 builds of one triple apart.
    pub(crate) amd64_variant: Option<String>,
    /// Pre-resolved `SOURCE_DATE_EPOCH` seconds. Resolved in the serial phase
    /// via `ctx.env_var` so the parallel execution phase never calls
    /// `std::env`; `None` outside a reproducible (harness) build.
    pub(crate) sde_epoch: Option<i64>,
}

/// Execute a prepared AppImage job: assemble the AppDir, then spawn
/// linuxdeploy with the constructed argv + env. Returns the registered
/// artifacts — always the `.AppImage`, plus its `.AppImage.zsync` sidecar when
/// `UPDATE_INFORMATION` produced one.
pub(crate) fn execute_appimage_job(
    job: &AppImageJob,
    verbosity: anodizer_core::log::Verbosity,
) -> Result<Vec<Artifact>> {
    let thread_log = anodizer_core::log::StageLogger::new("appimage", verbosity);

    // The root icon path is returned for parity with the staged layout but is
    // not handed to linuxdeploy: the icon is resolved from the pre-placed theme
    // tree (see [`linuxdeploy_args`]).
    let (desktop_dst, _icon_dst) = assemble_appdir(&job.appdir_root, job)?;

    // Reproducibility: pin every staged file's mtime to SOURCE_DATE_EPOCH so
    // the squashfs payload is byte-stable across runs (mirrors the sibling
    // makeself stage's mtime pinning). No-op outside a harness build.
    if let Some(epoch) = job.sde_epoch {
        pin_appdir_mtimes(&job.appdir_root, epoch)?;
    }

    // linuxdeploy runs with the AppDir's parent as cwd: it writes the output
    // `.AppImage` into the current dir, so a per-job dir keeps parallel jobs
    // from clobbering one another's output (each AppDir lives under
    // `dist/appimage/<id>/<platform>/`). The `--appdir` / `-d` paths it
    // receives MUST be relative to THAT cwd, not the process cwd: `appdir_root`
    // is `dist/appimage/<id>/<platform>/<app>.AppDir` relative to the worktree
    // root, and passing it verbatim while cwd is its own parent would re-resolve
    // it under work_dir and double the prefix — linuxdeploy then can't find the
    // staged AppDir / desktop file. The AppDir basename and
    // `<basename>/<desktop>` are the correct work_dir-relative forms.
    let work_dir = job.appdir_root.parent().unwrap_or(&job.appdir_root);
    let appdir_rel: PathBuf = job
        .appdir_root
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| job.appdir_root.clone());
    let desktop_rel = match desktop_dst.file_name() {
        Some(name) => appdir_rel.join(name),
        None => appdir_rel.clone(),
    };
    let args = linuxdeploy_args(&appdir_rel, &desktop_rel, &job.extra_args);
    let env = linuxdeploy_env(
        &job.version,
        &job.arch_token,
        &job.app_name,
        &job.filename,
        job.update_information.as_deref(),
    );

    thread_log.status(&format!("creating AppImage {}", job.filename));

    let mut command = Command::new("linuxdeploy");
    command.args(&args).current_dir(work_dir);
    for (k, v) in &env {
        command.env(k, v);
    }
    // Forward SOURCE_DATE_EPOCH so the child appimagetool stamps the squashfs
    // superblock deterministically (it honours the var when set, else falls
    // back to wall-clock). Resolved in the serial phase to keep this code
    // `std::env`-free.
    if let Some(epoch) = job.sde_epoch {
        command.env("SOURCE_DATE_EPOCH", epoch.to_string());
    }
    let output = command.output().with_context(|| {
        format!(
            "appimage: failed to spawn 'linuxdeploy' for {} (is linuxdeploy on PATH?)",
            job.filename
        )
    })?;

    if !output.status.success() {
        bail!(
            "appimage: linuxdeploy failed for '{}' (id={}): {}{}",
            job.filename,
            job.id,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // linuxdeploy writes the AppImage into the current working dir named after
    // APP/ARCH; locate it and move it to the deterministic output path.
    let built = locate_built_appimage(&job.appdir_root, &job.filename)?;
    if built != job.output_path {
        std::fs::rename(&built, &job.output_path)
            .or_else(|_| {
                std::fs::copy(&built, &job.output_path)?;
                std::fs::remove_file(&built)
            })
            .with_context(|| {
                format!(
                    "appimage: move {} → {}",
                    built.display(),
                    job.output_path.display()
                )
            })?;
    }

    // linuxdeploy-plugin-appimage emits a sidecar `<name>.AppImage.zsync` next
    // to the AppImage whenever UPDATE_INFORMATION is set (binary-delta update
    // metadata). Its `MTime:` header is stamped by `zsyncmake` from the
    // AppImage's *filesystem* mtime — wall-clock, unaffected by
    // SOURCE_DATE_EPOCH — so it drifts run-to-run and breaks determinism.
    // Rewrite it to the RFC 2822 rendering of SOURCE_DATE_EPOCH (and re-point
    // the Filename/URL fields if the AppImage was renamed on move). Only under
    // a reproducible build, and only when the sidecar exists.
    if let (Some(epoch), Some(_)) = (job.sde_epoch, job.update_information.as_ref()) {
        pin_zsync_mtime(&built, &job.output_path, epoch)?;
    }

    // The AppDir is linuxdeploy scaffolding, not a deliverable: its contents
    // are squashed into the `.AppImage` (the transitive determinism witness),
    // and nothing downstream reads the tree. Leaving it in `dist/` makes every
    // scaffolding file — including the hidden `.DirIcon` — a "shipped" artifact
    // that the determinism manifest records and publish-only hash-verifies.
    // `actions/upload-artifact` drops hidden files by default, so the preserved
    // dist that reaches publish is missing `.DirIcon` and hash-verify fails.
    // Remove the scaffolding once the AppImage (and its sidecar) are finalized.
    if job.appdir_root.exists() {
        std::fs::remove_dir_all(&job.appdir_root).with_context(|| {
            format!(
                "appimage: removing AppDir scaffolding {}",
                job.appdir_root.display()
            )
        })?;
    }

    let mut metadata = HashMap::new();
    metadata.insert("id".to_string(), job.id.clone());
    metadata.insert("format".to_string(), "appimage".to_string());
    metadata.insert("ext".to_string(), ".AppImage".to_string());
    if let Some(v) = &job.amd64_variant {
        metadata.insert("amd64_variant".to_string(), v.clone());
    }

    let mut artifacts = vec![Artifact {
        kind: ArtifactKind::AppImage,
        name: job.filename.clone(),
        path: job.output_path.clone(),
        target: job.primary_target.clone(),
        crate_name: job.primary_crate_name.clone(),
        metadata,
        size: None,
    }];

    // Register the `.AppImage.zsync` sidecar so it is byte-verified by the
    // determinism harness, preserved into the shard's dist, and uploaded to the
    // release. Without this the AppImage ships with embedded update-info
    // (`.upd_info`) pointing at a `*.AppImage.zsync` that never lands on the
    // release — delta auto-update silently 404s. `pin_zsync_mtime` has already
    // placed it at `<final>.AppImage.zsync` under a reproducible build (the
    // release/harness path always sets SOURCE_DATE_EPOCH); it is absent only
    // when no `UPDATE_INFORMATION` was configured, in which case the AppImage
    // carries no update pointer and needs no sidecar.
    let zsync_path = sidecar_zsync_path(&job.output_path);
    if zsync_path.is_file() {
        let mut zmeta = HashMap::new();
        zmeta.insert("id".to_string(), job.id.clone());
        zmeta.insert("format".to_string(), "appimage_zsync".to_string());
        zmeta.insert("ext".to_string(), ".AppImage.zsync".to_string());
        if let Some(v) = &job.amd64_variant {
            zmeta.insert("amd64_variant".to_string(), v.clone());
        }
        artifacts.push(Artifact {
            kind: ArtifactKind::UploadableFile,
            name: zsync_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            path: zsync_path,
            target: job.primary_target.clone(),
            crate_name: job.primary_crate_name.clone(),
            metadata: zmeta,
            size: None,
        });
    }

    Ok(artifacts)
}

/// The `.AppImage.zsync` sidecar path for a finalized `.AppImage` at
/// `appimage_path` (the sibling file `<name>.zsync`).
pub(crate) fn sidecar_zsync_path(appimage_path: &Path) -> PathBuf {
    let name = appimage_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    appimage_path.with_file_name(format!("{name}.zsync"))
}

/// Find the `.AppImage` linuxdeploy emitted in the cwd (the AppDir's parent),
/// where it writes `<APP>-<ARCH>.AppImage`.
///
/// Selection is deterministic and retry-safe: prefer an `.AppImage` whose
/// basename matches `expected_filename` (linuxdeploy's name is derived from
/// the same APP/ARCH that built `expected_filename`), then fall back to the
/// newest `.AppImage` by mtime. Picking the first `read_dir` entry would
/// otherwise be filesystem-order-dependent and could grab a stale output left
/// by a prior failed attempt in the same work dir.
pub(crate) fn locate_built_appimage(
    appdir_root: &Path,
    expected_filename: &str,
) -> Result<PathBuf> {
    let search_dir = appdir_root.parent().unwrap_or(appdir_root);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(search_dir)
        .with_context(|| format!("appimage: scan {} for output", search_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("AppImage") {
            candidates.push(path);
        }
    }

    if let Some(exact) = candidates
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(expected_filename))
    {
        return Ok(exact.clone());
    }

    // No exact match (linuxdeploy chose a different APP/ARCH casing): pick the
    // newest output so a stale `.AppImage` from a prior failed run is not
    // selected over the one just produced.
    candidates
        .into_iter()
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "appimage: linuxdeploy reported success but no .AppImage was found in {}",
                search_dir.display()
            )
        })
}

/// Pin a `.zsync` sidecar's `MTime:` header to `SOURCE_DATE_EPOCH` for
/// reproducibility, re-pointing `Filename:`/`URL:` if the AppImage was renamed.
///
/// `src_appimage` is the path linuxdeploy wrote the AppImage to (the sidecar
/// lives next to it as `<name>.zsync`); `final_appimage` is where the AppImage
/// was moved. `zsyncmake` records the AppImage's filesystem mtime (wall-clock)
/// in the `MTime:` header, so without this the sidecar's bytes drift run-to-run
/// even when the AppImage itself is byte-stable. The `SHA-1`/`Length` fields
/// checksum the AppImage bytes (unchanged here), so rewriting only the header
/// text keeps the sidecar valid.
///
/// A no-op when the sidecar is absent (no `UPDATE_INFORMATION` → no `.zsync`).
pub(crate) fn pin_zsync_mtime(
    src_appimage: &Path,
    final_appimage: &Path,
    epoch: i64,
) -> Result<()> {
    let src_name = src_appimage
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| {
            format!(
                "appimage: non-UTF8 AppImage name {}",
                src_appimage.display()
            )
        })?;
    let final_name = final_appimage
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| {
            format!(
                "appimage: non-UTF8 AppImage name {}",
                final_appimage.display()
            )
        })?;

    let zsync_src = src_appimage.with_file_name(format!("{src_name}.zsync"));
    if !zsync_src.exists() {
        return Ok(());
    }

    let mtime = anodizer_core::sde::rfc2822_utc_from_epoch(epoch).with_context(|| {
        format!("appimage: SOURCE_DATE_EPOCH {epoch} out of range for zsync MTime")
    })?;

    let bytes = std::fs::read(&zsync_src)
        .with_context(|| format!("appimage: read zsync {}", zsync_src.display()))?;
    // The header is newline-separated text terminated by a blank line
    // (`\n\n`); everything after that is the binary block-checksum table.
    let boundary = bytes
        .windows(2)
        .position(|w| w == b"\n\n")
        .with_context(|| {
            format!(
                "appimage: {} has no zsync header terminator",
                zsync_src.display()
            )
        })?;
    let header = std::str::from_utf8(&bytes[..boundary])
        .with_context(|| format!("appimage: {} header is not UTF8", zsync_src.display()))?;
    let body = &bytes[boundary..];

    let rename = src_name != final_name;
    let mut out = String::with_capacity(header.len());
    for (i, line) in header.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.starts_with("MTime:") {
            out.push_str(&format!("MTime: {mtime}"));
        } else if rename && line.starts_with("Filename:") {
            out.push_str(&format!("Filename: {final_name}"));
        } else if rename && line.starts_with("URL:") {
            out.push_str(&format!("URL: {final_name}"));
        } else {
            out.push_str(line);
        }
    }

    let mut new_bytes = out.into_bytes();
    new_bytes.extend_from_slice(body);

    let zsync_dst = final_appimage.with_file_name(format!("{final_name}.zsync"));
    std::fs::write(&zsync_dst, &new_bytes)
        .with_context(|| format!("appimage: write zsync {}", zsync_dst.display()))?;
    if zsync_dst != zsync_src {
        std::fs::remove_file(&zsync_src).ok();
    }
    Ok(())
}
