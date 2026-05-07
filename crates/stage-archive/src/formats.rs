//! Archive codec writers — `tar.gz`, `tar.xz`, `tar.zst`, `tar`, `gz`,
//! `zip`, plus the no-archive `binary` copy mode. Lifted out of the
//! ArchiveStage monolith so the per-codec logic is independently
//! reviewable and the lib root keeps to orchestration.

use std::fs::{self, File};
use std::io::{Read as IoRead, Write as IoWrite};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use chrono::DateTime;
use flate2::Compression;
use flate2::write::GzEncoder;

use crate::archive_log;

// ---------------------------------------------------------------------------
// parse_mtime
// ---------------------------------------------------------------------------

/// Parse an mtime string as either RFC3339 or a raw unix timestamp (u64).
/// Returns the unix timestamp as `u64`, or `None` if neither format matches.
pub(crate) fn parse_mtime(s: &str) -> Option<u64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp() as u64);
    }
    s.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// tar entry helpers
// ---------------------------------------------------------------------------

/// Apply `ArchiveFileInfo` overrides (mode, owner, group) to a tar header.
fn apply_file_info_to_header(
    header: &mut tar::Header,
    info: &anodizer_core::config::ArchiveFileInfo,
) {
    if let Some(mode) = info.mode {
        header.set_mode(mode.value());
    }
    if let Some(ref owner) = info.owner {
        header.set_username(owner).ok();
    }
    if let Some(ref group) = info.group {
        header.set_groupname(group).ok();
    }
    if let Some(ref mtime_str) = info.mtime {
        if let Some(ts) = parse_mtime(mtime_str) {
            header.set_mtime(ts);
        } else {
            archive_log().warn(&format!(
                "could not parse mtime '{mtime_str}' as RFC3339 or unix timestamp, ignoring"
            ));
        }
    }
}

/// Append a single file to a tar archive, optionally overriding mtime and
/// file info (mode/owner/group from builds_info).
/// When `mtime` is Some, a header is built manually with that timestamp so
/// that the archive is reproducible regardless of filesystem mtime.
pub(crate) fn append_tar_entry<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    archive_name: &Path,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    if mtime.is_some() || file_info.is_some() {
        let metadata =
            fs::metadata(src).with_context(|| format!("read metadata: {}", src.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&metadata);
        if let Some(ts) = mtime {
            header.set_mtime(ts);
            header.set_uid(0);
            header.set_gid(0);
            header.set_username("").ok();
            header.set_groupname("").ok();
        }
        if let Some(info) = file_info {
            apply_file_info_to_header(&mut header, info);
        }
        header
            .set_path(archive_name)
            .with_context(|| format!("set tar path: {}", archive_name.display()))?;
        header.set_cksum();
        let mut file = File::open(src).with_context(|| format!("open: {}", src.display()))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .with_context(|| format!("read: {}", src.display()))?;
        tar.append_data(&mut header, archive_name, data.as_slice())
            .with_context(|| format!("tar append: {}", archive_name.display()))?;
    } else {
        tar.append_path_with_name(src, archive_name)
            .with_context(|| format!("tar append: {}", archive_name.display()))?;
    }
    Ok(())
}

/// Shared tar archive creation: adds files to a tar builder, then finishes it.
/// When `file_info` is provided, it is applied to all entries (e.g. builds_info
/// permissions for binaries).
pub(crate) fn write_tar_entries<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    files: &[&Path],
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
    label: &str,
) -> Result<()> {
    for &src in files {
        if !src.exists() {
            continue;
        }
        let archive_name = compute_archive_name(src, base_dir, wrap_dir);
        append_tar_entry(tar, src, &archive_name, mtime, file_info).with_context(|| {
            format!(
                "{label}: adding {} as {}",
                src.display(),
                archive_name.display()
            )
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tar.gz / tar.xz / tar.zst / tar / gz / zip / binary writers
// ---------------------------------------------------------------------------

/// Create a tar.gz archive containing the given files.
/// Each file is stored under its own filename (no directory prefix) unless
/// `base_dir` is provided, in which case files are stored relative to it.
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
/// If `mtime` is provided, all entries are stored with that unix timestamp as mtime.
pub fn create_tar_gz(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.gz: {}", output.display()))?;
    let enc = GzEncoder::new(out_file, Compression::best());
    let mut tar = tar::Builder::new(enc);
    write_tar_entries(
        &mut tar, files, base_dir, wrap_dir, mtime, file_info, "tar.gz",
    )?;
    tar.finish().context("tar.gz: finish")
}

/// Create a tar.xz archive containing the given files.
pub fn create_tar_xz(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.xz: {}", output.display()))?;
    let enc = xz2::write::XzEncoder::new(out_file, 9);
    let mut tar = tar::Builder::new(enc);
    write_tar_entries(
        &mut tar, files, base_dir, wrap_dir, mtime, file_info, "tar.xz",
    )?;
    tar.finish().context("tar.xz: finish")
}

/// Create a tar.zst archive containing the given files.
pub fn create_tar_zst(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar.zst: {}", output.display()))?;
    // Level 3 is zstd's default, matching the Go zstd library used by
    // GoReleaser's archiver dependency. Previously level 19 (near-max) which
    // was much slower with marginal size improvement for release artifacts.
    let enc = zstd::Encoder::new(out_file, 3).context("tar.zst: create zstd encoder")?;
    let mut tar = tar::Builder::new(enc);
    write_tar_entries(
        &mut tar, files, base_dir, wrap_dir, mtime, file_info, "tar.zst",
    )?;
    let enc = tar.into_inner().context("tar.zst: finish tar")?;
    enc.finish().context("tar.zst: finish zstd")?;
    Ok(())
}

/// Create an uncompressed tar archive containing the given files.
pub fn create_tar(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar: {}", output.display()))?;
    let mut tar = tar::Builder::new(out_file);
    write_tar_entries(&mut tar, files, base_dir, wrap_dir, mtime, file_info, "tar")?;
    tar.finish().context("tar: finish")
}

/// Create a standalone .gz file from a single input file.
/// Unlike tar.gz, this compresses one file directly with gzip (gz cannot hold
/// multiple files without tar).
pub fn create_gz(file: &Path, output: &Path) -> Result<()> {
    if !file.exists() {
        bail!("gz: source file does not exist: {}", file.display());
    }
    let out_file =
        File::create(output).with_context(|| format!("create gz: {}", output.display()))?;
    let mut enc = GzEncoder::new(out_file, Compression::best());
    let data = fs::read(file).with_context(|| format!("gz: read {}", file.display()))?;
    enc.write_all(&data).context("gz: write compressed data")?;
    enc.finish().context("gz: finish")?;
    Ok(())
}

/// Create a standalone .xz file from a single input file.
/// Unlike tar.xz, this compresses one file directly with xz (xz cannot hold
/// multiple files without tar). Mirrors GoReleaser commit bb532b6 / #6520
/// (`pkg/archive/xz/xz.go`): the xz container is single-file, so callers
/// must dispatch with exactly one source. Error mirrors upstream's
/// `xz: failed to add %s, only one file can be archived in xz format`.
pub fn create_xz(file: &Path, output: &Path) -> Result<()> {
    if !file.exists() {
        bail!("xz: source file does not exist: {}", file.display());
    }
    let out_file =
        File::create(output).with_context(|| format!("create xz: {}", output.display()))?;
    let mut enc = xz2::write::XzEncoder::new(out_file, 9);
    let data = fs::read(file).with_context(|| format!("xz: read {}", file.display()))?;
    enc.write_all(&data).context("xz: write compressed data")?;
    enc.finish().context("xz: finish")?;
    Ok(())
}

/// Create a zip archive containing the given files.
/// Each file is stored under its own filename (no directory prefix).
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
/// If `file_info` is provided, unix permissions from `file_info.mode` are applied.
pub fn create_zip(
    files: &[&Path],
    output: &Path,
    wrap_dir: Option<&str>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create zip: {}", output.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    let mut options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    if let Some(info) = file_info
        && let Some(mode) = info.mode
    {
        options = options.unix_permissions(mode.value());
    }

    for &src in files {
        if !src.exists() {
            continue;
        }
        let base_name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let name = if let Some(dir) = wrap_dir {
            format!("{dir}/{base_name}")
        } else {
            base_name.to_string()
        };
        zip.start_file(&name, options)
            .with_context(|| format!("zip: start_file {name}"))?;
        let data = fs::read(src).with_context(|| format!("zip: read {}", src.display()))?;
        zip.write_all(&data)
            .with_context(|| format!("zip: write {name}"))?;
    }

    zip.finish().context("zip: finish")?;
    Ok(())
}

/// Copy binary files directly to the output directory (no archiving).
/// For a single file, copies to `output` directly.
/// For multiple files, copies each file into the parent directory of `output`.
pub fn copy_binary(files: &[&Path], output: &Path) -> Result<()> {
    if files.len() == 1 {
        let src = files[0];
        if !src.exists() {
            anyhow::bail!("binary: source does not exist: {}", src.display());
        }
        fs::copy(src, output)
            .with_context(|| format!("binary: copy {} -> {}", src.display(), output.display()))?;
    } else {
        let out_dir = output.parent().unwrap_or(Path::new("."));
        for &src in files {
            if !src.exists() {
                continue;
            }
            let file_name = src.file_name().unwrap_or(src.as_os_str());
            let dest = out_dir.join(file_name);
            fs::copy(src, &dest)
                .with_context(|| format!("binary: copy {} -> {}", src.display(), dest.display()))?;
        }
    }
    Ok(())
}

/// Normalize path separators: backslashes to forward slashes for archive entries.
/// Matches GoReleaser `archive.go:377`: `strings.ReplaceAll(..., "\\", "/")`.
pub(crate) fn normalize_archive_path(p: PathBuf) -> PathBuf {
    PathBuf::from(p.to_string_lossy().replace('\\', "/"))
}

/// Compute the archive entry name for a source file.
/// If `base_dir` is provided, the source is stored relative to it.
/// If `wrap_dir` is provided, the entry is prefixed with that directory name.
pub(crate) fn compute_archive_name(
    src: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
) -> PathBuf {
    let relative = if let Some(base) = base_dir {
        src.strip_prefix(base)
            .unwrap_or_else(|_| src.file_name().map(Path::new).unwrap_or(src))
            .to_path_buf()
    } else {
        PathBuf::from(src.file_name().unwrap_or(src.as_os_str()))
    };

    let joined = if let Some(dir) = wrap_dir {
        PathBuf::from(dir).join(relative)
    } else {
        relative
    };

    normalize_archive_path(joined)
}

/// Resolve a list of file patterns, expanding glob patterns.
/// Non-glob entries are treated as literal paths.
pub fn resolve_glob_patterns(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    for pattern in patterns {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let entries =
                glob::glob(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?;
            for entry in entries {
                let path = entry.with_context(|| format!("glob error for pattern: {pattern}"))?;
                results.push(path);
            }
        } else {
            let p = PathBuf::from(pattern);
            if p.exists() {
                results.push(p);
            }
        }
    }
    Ok(results)
}
