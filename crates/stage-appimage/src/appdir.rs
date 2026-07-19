use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::job::AppImageJob;

// ---------------------------------------------------------------------------
// AppDir assembly (pure FS — independently testable)
// ---------------------------------------------------------------------------

/// A file/dir to copy into the AppDir (`src` on disk → `dst` relative to the
/// AppDir root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppDirEntry {
    pub(crate) src: PathBuf,
    pub(crate) dst: String,
}

/// Assemble the AppDir on disk: copy the binary, desktop file, icon, the
/// harvested runtime tree (if any), and arbitrary extra entries into
/// `appdir`. Returns the absolute paths of the desktop file and icon as they
/// land inside the AppDir (linuxdeploy is pointed at the in-tree copies so a
/// per-run worktree prefix never leaks into its argv).
pub(crate) fn assemble_appdir(appdir: &Path, job: &AppImageJob) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(appdir)
        .with_context(|| format!("appimage: create AppDir {}", appdir.display()))?;

    // Binary → AppDir/usr/bin/<name>.
    let bin_dst = appdir.join("usr").join("bin").join(&job.binary_name);
    if let Some(parent) = bin_dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&job.binary_src, &bin_dst).with_context(|| {
        format!(
            "appimage: copy binary {} → {}",
            job.binary_src.display(),
            bin_dst.display()
        )
    })?;

    // Desktop file + icon land at the AppDir root (linuxdeploy moves them into
    // usr/share/applications and usr/share/icons during assembly).
    let desktop_dst = appdir.join(file_basename(&job.desktop_src, "app.desktop"));
    std::fs::copy(&job.desktop_src, &desktop_dst).with_context(|| {
        format!(
            "appimage: copy desktop file {} → {}",
            job.desktop_src.display(),
            desktop_dst.display()
        )
    })?;

    // Icon: deploy under the EXACT `Icon=` name from the desktop entry (so
    // linuxdeploy's theme resolution finds it) into both the icon-theme tree
    // and the AppDir root. See [`linuxdeploy_args`] for why the stage places
    // the icon itself instead of handing linuxdeploy a `-i` it may reject.
    let icon_ext = job
        .icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let icon_stem =
        desktop_icon_name(&desktop_dst).unwrap_or_else(|| file_stem(&job.icon_src, &job.app_name));
    let icon_file = format!("{icon_stem}.{icon_ext}");

    let theme_apps_dir = appdir
        .join("usr")
        .join("share")
        .join("icons")
        .join("hicolor")
        .join(icon_theme_subdir(&job.icon_src))
        .join("apps");
    std::fs::create_dir_all(&theme_apps_dir).with_context(|| {
        format!(
            "appimage: create icon theme dir {}",
            theme_apps_dir.display()
        )
    })?;
    let theme_icon = theme_apps_dir.join(&icon_file);
    std::fs::copy(&job.icon_src, &theme_icon).with_context(|| {
        format!(
            "appimage: copy icon {} → {}",
            job.icon_src.display(),
            theme_icon.display()
        )
    })?;

    // Root copy: linuxdeploy symlinks `.DirIcon` from the top-level icon, so a
    // root copy named for the `Icon=` key matches its own successful-run layout.
    let icon_dst = appdir.join(&icon_file);
    std::fs::copy(&job.icon_src, &icon_dst).with_context(|| {
        format!(
            "appimage: copy icon {} → {}",
            job.icon_src.display(),
            icon_dst.display()
        )
    })?;

    for entry in &job.appdir_entries {
        copy_entry_into_appdir(appdir, entry)?;
    }

    Ok((desktop_dst, icon_dst))
}

/// Extract the `Icon=` value from a `.desktop` file so the AppDir icon is
/// deployed under the exact name linuxdeploy resolves the entry against.
///
/// Returns the trimmed value with any trailing image extension stripped (the
/// freedesktop `Icon=` key is conventionally a bare name); `None` when the file
/// is unreadable or carries no non-empty `Icon=` key, in which case the caller
/// falls back to a derived stem.
pub(crate) fn desktop_icon_name(desktop: &Path) -> Option<String> {
    let text = std::fs::read_to_string(desktop).ok()?;
    let raw = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Icon="))?
        .trim();
    let name = Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw)
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The freedesktop icon-theme subdirectory an icon belongs under: `scalable`
/// for SVG, else `<W>x<H>` read from the PNG header, falling back to `256x256`
/// when the dimensions can't be read.
///
/// linuxdeploy does not resolution-validate an icon already present in the
/// AppDir's theme tree (unlike one passed via `-i`), so a non-standard size
/// such as `1024x1024` is deployed verbatim — the property that lets the stage
/// accept any source icon resolution.
pub(crate) fn icon_theme_subdir(icon_src: &Path) -> String {
    let ext = icon_src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "svg" || ext == "svgz" {
        return "scalable".to_string();
    }
    png_dimensions(icon_src)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|| "256x256".to_string())
}

/// Read a PNG's pixel dimensions from its IHDR chunk (width then height, both
/// big-endian u32 immediately after the 8-byte signature + 8-byte chunk
/// header). `None` for a non-PNG or a file too short to carry an IHDR.
///
/// No image-decoding dependency: the IHDR sits at a fixed offset, so the two
/// u32s are a direct slice read.
pub(crate) fn png_dimensions(path: &Path) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 24 || &bytes[0..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// The file stem of `path` (basename without extension), or `fallback` when it
/// has none.
fn file_stem(path: &Path, fallback: &str) -> String {
    path.file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback)
        .to_string()
}

/// Copy one [`AppDirEntry`] (file or directory) into the AppDir at its `dst`.
fn copy_entry_into_appdir(appdir: &Path, entry: &AppDirEntry) -> Result<()> {
    let dst = appdir.join(&entry.dst);
    if entry.src.is_dir() {
        anodizer_core::util::copy_dir_tree(&entry.src, &dst).with_context(|| {
            format!(
                "appimage: copy dir {} → {}",
                entry.src.display(),
                dst.display()
            )
        })?;
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&entry.src, &dst).with_context(|| {
            format!(
                "appimage: copy file {} → {}",
                entry.src.display(),
                dst.display()
            )
        })?;
    }
    Ok(())
}

/// Recursively pin every regular file's mtime under `dir` to `epoch_secs`.
///
/// linuxdeploy's `appimage` output plugin wraps the AppDir in a squashfs;
/// squashfs embeds each file's on-disk mtime. `fs::copy` (in
/// [`assemble_appdir`]) stamps every staged file with the current wall-clock,
/// so two runs produce identical content with different timestamps and the
/// resulting `.AppImage` bytes drift. Pinning the AppDir tree to
/// `SOURCE_DATE_EPOCH` removes that drift; appimagetool itself honours
/// `SOURCE_DATE_EPOCH` for the squashfs superblock when the env var is set.
pub(crate) fn pin_appdir_mtimes(dir: &Path, epoch_secs: i64) -> Result<()> {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in std::fs::read_dir(&p)
            .with_context(|| format!("appimage: read_dir {} for mtime pin", p.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                anodizer_core::util::set_file_mtime_epoch(&path, epoch_secs)
                    .with_context(|| format!("appimage: pin mtime on {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// The basename of `path`, or `fallback` when it has none.
fn file_basename(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback)
        .to_string()
}
