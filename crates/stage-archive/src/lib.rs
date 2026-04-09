use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read as IoRead, Write as IoWrite};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use chrono::DateTime;
use flate2::Compression;
use flate2::write::GzEncoder;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{
    ArchiveConfig, ArchiveFileSpec, ArchivesConfig, FormatOverride, VALID_ARCHIVE_FORMATS,
    parse_octal_mode,
};
use anodize_core::context::Context;
use anodize_core::hooks::run_hooks;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

// ---------------------------------------------------------------------------
// parse_mtime  (helper)
// ---------------------------------------------------------------------------

/// Parse an mtime string as either RFC3339 or a raw unix timestamp (u64).
/// Returns the unix timestamp as `u64`, or `None` if neither format matches.
fn parse_mtime(s: &str) -> Option<u64> {
    // Try RFC3339 first (e.g. "2023-11-14T22:13:20+00:00")
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp() as u64);
    }
    // Fall back to raw unix timestamp
    s.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// append_tar_entry  (helper)
// ---------------------------------------------------------------------------

/// Apply `ArchiveFileInfo` overrides (mode, owner, group) to a tar header.
fn apply_file_info_to_header(
    header: &mut tar::Header,
    info: &anodize_core::config::ArchiveFileInfo,
) {
    if let Some(mode_str) = &info.mode
        && let Some(mode) = parse_octal_mode(mode_str)
    {
        header.set_mode(mode);
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
            eprintln!(
                "Warning: [archive] could not parse mtime '{}' as RFC3339 or unix timestamp, ignoring",
                mtime_str
            );
        }
    }
}

/// Append a single file to a tar archive, optionally overriding mtime and
/// file info (mode/owner/group from builds_info).
/// When `mtime` is Some, a header is built manually with that timestamp so
/// that the archive is reproducible regardless of filesystem mtime.
fn append_tar_entry<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    archive_name: &Path,
    mtime: Option<u64>,
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
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

// ---------------------------------------------------------------------------
// create_tar_gz
// ---------------------------------------------------------------------------

/// Shared tar archive creation: adds files to a tar builder, then finishes it.
/// When `file_info` is provided, it is applied to all entries (e.g. builds_info
/// permissions for binaries).
fn write_tar_entries<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    files: &[&Path],
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
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
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
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
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
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
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
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
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create tar: {}", output.display()))?;
    let mut tar = tar::Builder::new(out_file);
    write_tar_entries(&mut tar, files, base_dir, wrap_dir, mtime, file_info, "tar")?;
    tar.finish().context("tar: finish")
}

// ---------------------------------------------------------------------------
// create_gz  (standalone gzip, no tar wrapping)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// create_zip
// ---------------------------------------------------------------------------

/// Create a zip archive containing the given files.
/// Each file is stored under its own filename (no directory prefix).
/// If `wrap_dir` is provided, all archive entries are prefixed with that directory.
/// If `file_info` is provided, unix permissions from `file_info.mode` are applied.
pub fn create_zip(
    files: &[&Path],
    output: &Path,
    wrap_dir: Option<&str>,
    file_info: Option<&anodize_core::config::ArchiveFileInfo>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create zip: {}", output.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    let mut options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Apply unix permissions from file_info if set
    if let Some(info) = file_info
        && let Some(mode_str) = &info.mode
        && let Some(mode) = parse_octal_mode(mode_str)
    {
        options = options.unix_permissions(mode);
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

// ---------------------------------------------------------------------------
// copy_binary
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// resolve_glob_patterns
// ---------------------------------------------------------------------------

/// Resolve a list of file patterns, expanding glob patterns.
/// Non-glob entries are treated as literal paths.
pub fn resolve_glob_patterns(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    for pattern in patterns {
        // Check if the pattern contains glob metacharacters
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

// ---------------------------------------------------------------------------
// longest_common_prefix — used for glob directory preservation
// ---------------------------------------------------------------------------

/// Compute the longest common byte prefix of a slice of strings.
/// Returns an empty string when the slice is empty.
fn longest_common_prefix(strs: &[String]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut lcp = strs[0].as_str();
    for s in &strs[1..] {
        let end = lcp
            .bytes()
            .zip(s.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        lcp = &lcp[..end];
    }
    lcp.to_string()
}

// ---------------------------------------------------------------------------
// render_file_info — template-render owner/group/mtime in ArchiveFileInfo
// ---------------------------------------------------------------------------

/// Render template expressions in `ArchiveFileInfo` fields.
///
/// GoReleaser processes `owner`, `group`, and `mtime` through its template
/// engine (archivefiles.go `tmplInfo()`). `mode` is an octal literal and is
/// passed through unchanged.
fn render_file_info(
    info: &anodize_core::config::ArchiveFileInfo,
    ctx: &Context,
) -> Result<anodize_core::config::ArchiveFileInfo> {
    Ok(anodize_core::config::ArchiveFileInfo {
        owner: info
            .owner
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        group: info
            .group
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        mode: info.mode.clone(),
        mtime: info
            .mtime
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
    })
}

// ---------------------------------------------------------------------------
// resolve_file_specs — handle ArchiveFileSpec entries
// ---------------------------------------------------------------------------

/// A resolved extra file to include in an archive, with optional destination
/// path override and file info (permissions/owner/group).
pub struct ResolvedExtraFile {
    pub src: PathBuf,
    /// When Some, use this path inside the archive instead of the filename.
    pub dst: Option<String>,
    /// File metadata to apply to the archive entry.
    pub info: Option<anodize_core::config::ArchiveFileInfo>,
    /// When true, strip the parent directory from the source path so the file
    /// is placed at the archive root (or directly under wrap_in_directory).
    pub strip_parent: bool,
}

/// Resolve a list of ArchiveFileSpec entries into concrete file paths with
/// optional destination overrides and file info.
pub fn resolve_file_specs(specs: &[ArchiveFileSpec]) -> Result<Vec<ResolvedExtraFile>> {
    let mut results = Vec::new();
    for spec in specs {
        match spec {
            ArchiveFileSpec::Glob(pattern) => {
                let paths = resolve_glob_patterns(std::slice::from_ref(pattern))?;
                for p in paths {
                    results.push(ResolvedExtraFile {
                        src: p,
                        dst: None,
                        info: None,
                        strip_parent: false,
                    });
                }
            }
            ArchiveFileSpec::Detailed {
                src,
                dst,
                info,
                strip_parent,
            } => {
                let paths = resolve_glob_patterns(std::slice::from_ref(src))?;
                let do_strip = strip_parent.unwrap_or(false);

                // When dst is set and strip_parent is false, compute per-file
                // destinations that preserve the directory structure relative
                // to the longest common prefix of all matched paths.
                //
                // GoReleaser divergence: when src is a non-glob literal,
                // GoReleaser uses it as the prefix directly, so
                // Rel(file, file) = "." and the file is effectively renamed
                // to dst. We always compute LCP, which for a single file
                // produces dst/filename — more intuitive behavior (e.g.
                // dst: "licenses/" puts the file inside a licenses directory
                // rather than renaming it).
                if dst.is_some() && !do_strip {
                    let dst_prefix = dst.as_deref().unwrap();
                    let file_strs: Vec<String> = paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();

                    // Compute prefix directory: use the LCP of matched paths,
                    // then take its parent directory if it's not an existing
                    // directory (inspired by GoReleaser's filepath.Dir fallback).
                    let lcp = longest_common_prefix(&file_strs);
                    let prefix_dir = {
                        let lcp_path = std::path::Path::new(&lcp);
                        if lcp_path.is_dir() {
                            lcp_path.to_path_buf()
                        } else {
                            lcp_path
                                .parent()
                                .unwrap_or_else(|| std::path::Path::new(""))
                                .to_path_buf()
                        }
                    };

                    for p in paths {
                        let rel = p
                            .strip_prefix(&prefix_dir)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .to_string();
                        // Normalize to forward slashes — archive entry paths must
                        // always use '/' regardless of platform.
                        let dest =
                            normalize_archive_path(std::path::PathBuf::from(dst_prefix).join(&rel))
                                .to_string_lossy()
                                .to_string();
                        results.push(ResolvedExtraFile {
                            src: p,
                            dst: Some(dest),
                            info: info.clone(),
                            strip_parent: false,
                        });
                    }
                } else {
                    for p in paths {
                        results.push(ResolvedExtraFile {
                            src: p,
                            dst: dst.clone(),
                            info: info.clone(),
                            strip_parent: do_strip,
                        });
                    }
                }
            }
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// resolve_default_extra_files — auto-include common files when none configured
// ---------------------------------------------------------------------------

/// When no extra files are explicitly configured, glob for common project files
/// (LICENSE, README, CHANGELOG) in the current directory, matching GoReleaser's
/// Default() behavior. Non-matching patterns are silently skipped.
fn resolve_default_extra_files() -> Vec<ResolvedExtraFile> {
    let patterns = [
        "LICENSE*",
        "license*",
        "README*",
        "readme*",
        "CHANGELOG*",
        "changelog*",
    ];
    let mut results = Vec::new();
    for pattern in &patterns {
        if let Ok(entries) = glob::glob(pattern) {
            for entry in entries.flatten() {
                // Avoid duplicates (e.g. LICENSE matched by both LICENSE* and license*)
                if !results.iter().any(|r: &ResolvedExtraFile| r.src == entry) {
                    results.push(ResolvedExtraFile {
                        src: entry,
                        dst: None,
                        info: None,
                        strip_parent: false,
                    });
                }
            }
        }
    }
    results
}

/// Normalize path separators: backslashes to forward slashes for archive entries.
/// Matches GoReleaser `archive.go:377`: `strings.ReplaceAll(..., "\\", "/")`.
fn normalize_archive_path(p: PathBuf) -> PathBuf {
    PathBuf::from(p.to_string_lossy().replace('\\', "/"))
}

// ---------------------------------------------------------------------------
// compute_archive_name  (helper)
// ---------------------------------------------------------------------------

/// Compute the archive entry name for a source file.
/// If `base_dir` is provided, the source is stored relative to it.
/// If `wrap_dir` is provided, the entry is prefixed with that directory name.
fn compute_archive_name(src: &Path, base_dir: Option<&Path>, wrap_dir: Option<&str>) -> PathBuf {
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

// ---------------------------------------------------------------------------
// ArchiveEntry — rich file descriptor for archive creation
// ---------------------------------------------------------------------------

/// An entry to add to an archive, carrying source path, archive-internal name,
/// and optional per-file info (permissions/owner/group).
struct ArchiveEntry {
    /// Source file path on disk.
    src: PathBuf,
    /// Name inside the archive (may differ from src filename due to dst override).
    archive_name: PathBuf,
    /// Per-file info overrides (mode/owner/group/mtime).
    info: Option<anodize_core::config::ArchiveFileInfo>,
}

// ---------------------------------------------------------------------------
// deduplicate_entries — warn + skip when the same archive path appears twice
// ---------------------------------------------------------------------------

/// Deduplicate archive entries by `archive_name`. When a duplicate destination
/// path is found, emit a warning to stderr and keep only the first occurrence.
/// This matches GoReleaser's `unique()` in archivefiles.go.
fn deduplicate_entries(entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(first_src) = seen.get(&entry.archive_name) {
            eprintln!(
                "Warning: [archive] file '{}' already exists in archive as '{}' — '{}' will be ignored",
                entry.archive_name.display(),
                first_src.display(),
                entry.src.display(),
            );
        } else {
            seen.insert(entry.archive_name.clone(), entry.src.clone());
            result.push(entry);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// sort_entries — deterministic archive order for reproducibility
// ---------------------------------------------------------------------------

/// Sort archive entries by `archive_name` to ensure deterministic ordering.
/// This matches GoReleaser's sort in archivefiles.go:66-68 and is essential
/// for reproducible archives.
fn sort_entries(mut entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    entries.sort_by(|a, b| a.archive_name.cmp(&b.archive_name));
    entries
}

/// Write a list of `ArchiveEntry` items into a tar builder, applying per-entry
/// file info and an optional global mtime.
fn write_archive_entries<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    entries: &[ArchiveEntry],
    mtime: Option<u64>,
    label: &str,
) -> Result<()> {
    for entry in entries {
        if !entry.src.exists() {
            continue;
        }
        append_tar_entry(
            tar,
            &entry.src,
            &entry.archive_name,
            mtime,
            entry.info.as_ref(),
        )
        .with_context(|| {
            format!(
                "{label}: adding {} as {}",
                entry.src.display(),
                entry.archive_name.display()
            )
        })?;
    }
    Ok(())
}

/// Convert a unix timestamp to a `zip::DateTime` for setting zip entry mtime.
fn unix_timestamp_to_zip_datetime(ts: u64) -> Option<zip::DateTime> {
    use chrono::{TimeZone, Utc};
    let dt = Utc.timestamp_opt(ts as i64, 0).single()?;
    zip::DateTime::from_date_and_time(
        dt.format("%Y").to_string().parse::<u16>().ok()?,
        dt.format("%m").to_string().parse::<u8>().ok()?,
        dt.format("%d").to_string().parse::<u8>().ok()?,
        dt.format("%H").to_string().parse::<u8>().ok()?,
        dt.format("%M").to_string().parse::<u8>().ok()?,
        dt.format("%S").to_string().parse::<u8>().ok()?,
    )
    .ok()
}

/// Write a list of `ArchiveEntry` items into a zip writer, applying per-entry
/// file info (unix permissions from mode) and optional mtime for reproducible builds.
fn write_zip_entries<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    entries: &[ArchiveEntry],
    mtime: Option<u64>,
) -> Result<()> {
    let mut base_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Apply mtime for reproducible builds
    if let Some(ts) = mtime
        && let Some(zip_dt) = unix_timestamp_to_zip_datetime(ts)
    {
        base_options = base_options.last_modified_time(zip_dt);
    }

    for entry in entries {
        if !entry.src.exists() {
            continue;
        }
        let mut options = base_options;
        if let Some(ref info) = entry.info
            && let Some(mode_str) = &info.mode
            && let Some(mode) = parse_octal_mode(mode_str)
        {
            options = options.unix_permissions(mode);
        }
        let name = entry.archive_name.to_string_lossy().to_string();
        zip.start_file(&name, options)
            .with_context(|| format!("zip: start_file {name}"))?;
        let data =
            fs::read(&entry.src).with_context(|| format!("zip: read {}", entry.src.display()))?;
        zip.write_all(&data)
            .with_context(|| format!("zip: write {name}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// format_for_target
// ---------------------------------------------------------------------------

/// Determine the archive format(s) for a target, applying OS-based overrides.
/// When a FormatOverride has `formats` (plural), returns all of them.
/// Otherwise uses the singular `format` field.
/// Falls back to `default_format` when no override matches.
pub fn formats_for_target(
    target: &str,
    default_format: &str,
    overrides: &[FormatOverride],
) -> Vec<String> {
    let (os, _arch) = map_target(target);
    for ov in overrides {
        if ov.os == os {
            // Plural takes priority over singular
            if let Some(ref fmts) = ov.formats
                && !fmts.is_empty()
            {
                return fmts.clone();
            }
            if let Some(ref fmt) = ov.format {
                return vec![fmt.clone()];
            }
        }
    }
    vec![default_format.to_string()]
}

/// Determine the archive format for a target (returns the first match).
/// Convenience wrapper around `formats_for_target`.
pub fn format_for_target(
    target: &str,
    default_format: &str,
    overrides: &[FormatOverride],
) -> String {
    formats_for_target(target, default_format, overrides)
        .into_iter()
        .next()
        .unwrap_or_else(|| default_format.to_string())
}

// ---------------------------------------------------------------------------
// default_name_template
// ---------------------------------------------------------------------------

fn default_name_template() -> &'static str {
    "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

fn default_binary_name_template() -> &'static str {
    "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

// ---------------------------------------------------------------------------
// ArchiveStage
// ---------------------------------------------------------------------------

pub struct ArchiveStage;

impl Stage for ArchiveStage {
    fn name(&self) -> &str {
        "archive"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("archive");
        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let template_vars = ctx.template_vars().clone();

        // Global archive defaults
        let global_default_format = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.format.clone())
            .unwrap_or_else(|| "tar.gz".to_string());
        let global_format_overrides: Vec<FormatOverride> = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.archives.as_ref())
            .and_then(|a| a.format_overrides.clone())
            .unwrap_or_default();

        // Collect crate configs to avoid borrow conflict later
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Build a list of (crate_name, archive_configs) pairs to process
        let work: Vec<(String, Vec<ArchiveConfig>)> = crates
            .into_iter()
            .filter_map(|c| {
                match &c.archives {
                    ArchivesConfig::Disabled => None,
                    ArchivesConfig::Configs(cfgs) => {
                        let archive_cfgs = if cfgs.is_empty() {
                            // Default: one archive with all defaults
                            vec![ArchiveConfig::default()]
                        } else {
                            cfgs.clone()
                        };
                        Some((c.name.clone(), archive_cfgs))
                    }
                }
            })
            .collect();

        // Early validation: reject unknown archive format strings before doing
        // any I/O so typos are surfaced immediately.
        for (_crate_name, archive_cfgs) in &work {
            for cfg in archive_cfgs {
                if let Some(ref fmt) = cfg.format
                    && !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str())
                {
                    bail!(
                        "unsupported archive format: {fmt} (valid: {})",
                        VALID_ARCHIVE_FORMATS.join(", ")
                    );
                }
                if let Some(ref fmts) = cfg.formats {
                    for fmt in fmts {
                        if !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str()) {
                            bail!(
                                "unsupported archive format: {fmt} (valid: {})",
                                VALID_ARCHIVE_FORMATS.join(", ")
                            );
                        }
                    }
                }
                if let Some(ref overrides) = cfg.format_overrides {
                    for ov in overrides {
                        // GoReleaser warns when format_overrides entries have
                        // empty goos or empty format.
                        if ov.os.is_empty() {
                            log.warn("format_override has empty goos/os value");
                        }
                        if ov.format.as_deref().is_none_or(str::is_empty)
                            && ov.formats.as_ref().is_none_or(|f| f.is_empty())
                        {
                            log.warn("format_override has empty format value");
                        }
                        if let Some(ref fmt) = ov.format
                            && !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str())
                        {
                            bail!(
                                "unsupported archive format: {fmt} (valid: {})",
                                VALID_ARCHIVE_FORMATS.join(", ")
                            );
                        }
                        if let Some(ref fmts) = ov.formats {
                            for fmt in fmts {
                                if !VALID_ARCHIVE_FORMATS.contains(&fmt.as_str()) {
                                    bail!(
                                        "unsupported archive format: {fmt} (valid: {})",
                                        VALID_ARCHIVE_FORMATS.join(", ")
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Ensure dist directory exists
        fs::create_dir_all(&dist)
            .with_context(|| format!("create dist dir: {}", dist.display()))?;

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for (crate_name, archive_cfgs) in &work {
            // Archive all build artifact types, matching GoReleaser
            // (Binary, UniversalBinary, Header, CArchive, CShared).
            let archivable_kinds = [
                ArtifactKind::Binary,
                ArtifactKind::UniversalBinary,
                ArtifactKind::Header,
                ArtifactKind::CArchive,
                ArtifactKind::CShared,
            ];
            let all_binaries: Vec<Artifact> = ctx
                .artifacts
                .by_kinds_and_crate(&archivable_kinds, crate_name)
                .into_iter()
                .cloned()
                .collect();

            // meta archives can skip the "no binaries" check
            let has_any_meta = archive_cfgs.iter().any(|cfg| cfg.meta.unwrap_or(false));

            if all_binaries.is_empty() && !has_any_meta {
                log.warn(&format!("no binaries for crate {crate_name}, skipping"));
                continue;
            }

            for archive_cfg in archive_cfgs {
                let is_meta = archive_cfg.meta.unwrap_or(false);

                // ids filtering: only include binary artifacts whose metadata
                // "id" matches one of the listed IDs (same pattern as checksum)
                let binaries: Vec<Artifact> = if is_meta {
                    // Meta archives have no binaries
                    Vec::new()
                } else if let Some(ref ids) = archive_cfg.ids {
                    all_binaries
                        .iter()
                        .filter(|a| matches!(a.metadata.get("id"), Some(id) if ids.contains(id)))
                        .cloned()
                        .collect()
                } else {
                    all_binaries.clone()
                };

                if binaries.is_empty() && !is_meta {
                    continue;
                }

                // Group binaries by target
                let mut by_target: HashMap<String, Vec<Artifact>> = HashMap::new();
                for bin in &binaries {
                    let target = bin.target.clone().unwrap_or_else(|| "unknown".to_string());
                    by_target.entry(target).or_default().push(bin.clone());
                }

                // For meta archives with no binaries, create a single entry with "unknown" target
                if is_meta && by_target.is_empty() {
                    by_target.insert("unknown".to_string(), Vec::new());
                }

                // allow_different_binary_count check: when false (default),
                // error if different targets have different binary counts
                // (matches GoReleaser behavior which errors, not warns).
                // GoReleaser exempts the "binary" format from this check
                // (archive/archive.go:131).
                let archive_format = archive_cfg.format.as_deref().unwrap_or("tar.gz");
                if archive_format != "binary"
                    && !archive_cfg.allow_different_binary_count.unwrap_or(false)
                    && by_target.len() > 1
                {
                    let counts: Vec<usize> = by_target.values().map(|bins| bins.len()).collect();
                    let first = counts[0];
                    if counts.iter().any(|&c| c != first) {
                        let details: Vec<_> = by_target
                            .iter()
                            .map(|(t, b)| format!("{t}={}", b.len()))
                            .collect();
                        bail!(
                            "binary counts differ across targets ({:?}); set allow_different_binary_count: true to allow this",
                            details
                        );
                    }
                }

                // Determine singular default format (per-config > global default)
                let singular_format = archive_cfg
                    .format
                    .as_deref()
                    .unwrap_or(&global_default_format);

                // Determine format overrides (per-config > global)
                let format_overrides: Vec<FormatOverride> = archive_cfg
                    .format_overrides
                    .clone()
                    .unwrap_or_else(|| global_format_overrides.clone());

                // Determine which binaries to include
                let binary_filter: Option<&Vec<String>> = archive_cfg.binaries.as_ref();

                // Name template
                let has_custom_name_tmpl = archive_cfg.name_template.is_some();
                let default_tmpl = default_name_template();
                let name_tmpl = archive_cfg.name_template.as_deref().unwrap_or(default_tmpl);

                // strip_binary_directory: place binaries at archive root
                let strip_bin_dir = archive_cfg.strip_binary_directory.unwrap_or(false);

                // Pre-archive hooks
                if let Some(pre) = archive_cfg.hooks.as_ref().and_then(|h| h.pre.as_ref()) {
                    run_hooks(pre, "pre-archive", dry_run, &log, Some(&template_vars))?;
                }

                for (target, target_bins) in &by_target {
                    // Filter binaries for this archive config
                    let selected_bins: Vec<&Artifact> = target_bins
                        .iter()
                        .filter(|b| match binary_filter {
                            None => true,
                            Some(names) => {
                                let bin_name =
                                    b.metadata.get("binary").map(|s| s.as_str()).unwrap_or("");
                                names.iter().any(|n| n == bin_name)
                            }
                        })
                        .collect();

                    if selected_bins.is_empty() && !is_meta {
                        continue;
                    }

                    // Determine the list of formats to produce for this target.
                    // If `formats` (plural) is set and non-empty, use it exactly as
                    // specified (ignore both singular `format` and `format_overrides`).
                    // Otherwise, fall back to the singular format with per-target
                    // overrides (which may themselves produce multiple formats via
                    // FormatOverride.formats plural).
                    let formats_to_produce: Vec<String> = match &archive_cfg.formats {
                        Some(fmts) if !fmts.is_empty() => fmts.clone(),
                        _ => formats_for_target(target, singular_format, &format_overrides),
                    };

                    let (os, arch) = map_target(target);

                    // Build template vars for this target
                    let tvars = ctx.template_vars_mut();
                    tvars.set("Os", &os);
                    tvars.set("Arch", &arch);
                    tvars.set("Target", target);

                    // Set Binary to the first selected binary's name (matches GoReleaser behavior)
                    if let Some(bin_name) =
                        selected_bins.first().and_then(|b| b.metadata.get("binary"))
                    {
                        tvars.set("Binary", bin_name);
                    }

                    // Render name
                    let archive_stem = ctx.render_template(name_tmpl).with_context(|| {
                        format!("render archive name for {crate_name}/{target}")
                    })?;

                    // Render wrap_in_directory (template-aware)
                    // WrapInDirectory::Bool(true)  -> use the archive stem as the wrap dir
                    // WrapInDirectory::Bool(false) -> no wrapping
                    // WrapInDirectory::Name(s)     -> treat as a template string to render
                    let wrap_dir_rendered = if let Some(ref wid) = archive_cfg.wrap_in_directory {
                        match wid {
                            anodize_core::config::WrapInDirectory::Bool(true) => {
                                Some(archive_stem.clone())
                            }
                            anodize_core::config::WrapInDirectory::Bool(false) => None,
                            anodize_core::config::WrapInDirectory::Name(tmpl) => {
                                if tmpl.is_empty() {
                                    None
                                } else {
                                    Some(ctx.render_template(tmpl).with_context(|| {
                                        format!(
                                            "render wrap_in_directory for {crate_name}/{target}"
                                        )
                                    })?)
                                }
                            }
                        }
                    } else {
                        None
                    };
                    let wrap_dir = wrap_dir_rendered.as_deref();

                    // Collect binary files — unless meta archive
                    let mut binary_paths: Vec<PathBuf> = Vec::new();
                    if !is_meta {
                        for b in &selected_bins {
                            if !b.path.exists() && !dry_run {
                                anyhow::bail!(
                                    "binary artifact missing: {} (expected at {})",
                                    b.metadata.get("binary").unwrap_or(&b.crate_name),
                                    b.path.display()
                                );
                            }
                            binary_paths.push(b.path.clone());
                        }
                    }

                    // Extra files (LICENSE, README, etc.) — with ArchiveFileSpec support.
                    // When no files are configured, auto-include common files
                    // (LICENSE*, README*, CHANGELOG*) matching GoReleaser defaults.
                    // GoReleaser renders file spec source patterns through the
                    // template engine before glob expansion.
                    let extra_files: Vec<ResolvedExtraFile> = if let Some(file_specs) =
                        &archive_cfg.files
                    {
                        let rendered_specs: Vec<ArchiveFileSpec> = file_specs
                            .iter()
                            .map(|spec| match spec {
                                ArchiveFileSpec::Glob(pattern) => {
                                    let rendered = ctx
                                        .render_template(pattern)
                                        .unwrap_or_else(|_| pattern.clone());
                                    ArchiveFileSpec::Glob(rendered)
                                }
                                ArchiveFileSpec::Detailed {
                                    src,
                                    dst,
                                    info,
                                    strip_parent,
                                } => {
                                    let rendered_src =
                                        ctx.render_template(src).unwrap_or_else(|_| src.clone());
                                    ArchiveFileSpec::Detailed {
                                        src: rendered_src,
                                        dst: dst.clone(),
                                        info: info.clone(),
                                        strip_parent: *strip_parent,
                                    }
                                }
                            })
                            .collect();
                        resolve_file_specs(&rendered_specs).with_context(|| {
                            format!("resolve file specs for {crate_name}/{target}")
                        })?
                    } else {
                        resolve_default_extra_files()
                    };

                    // builds_info: permissions applied to binary entries.
                    // Default binary permissions to 0o755 (executable) when no
                    // explicit builds_info is configured, matching GoReleaser.
                    let binary_info = archive_cfg.builds_info.clone().unwrap_or_else(|| {
                        anodize_core::config::ArchiveFileInfo {
                            mode: Some("0755".to_string()),
                            ..Default::default()
                        }
                    });
                    let binary_info = render_file_info(&binary_info, ctx)?;

                    // Build ArchiveEntry items for binaries.
                    // strip_binary_directory: when true, binaries skip the
                    // wrap_in_directory prefix (placed at archive root).
                    let binary_entries: Vec<ArchiveEntry> = binary_paths
                        .iter()
                        .map(|bp| {
                            let file_name = bp
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| "unknown".to_string());
                            let raw_name = if strip_bin_dir {
                                PathBuf::from(&file_name)
                            } else if let Some(dir) = wrap_dir {
                                PathBuf::from(dir).join(&file_name)
                            } else {
                                PathBuf::from(&file_name)
                            };
                            let archive_name = normalize_archive_path(raw_name);
                            ArchiveEntry {
                                src: bp.clone(),
                                archive_name,
                                info: Some(binary_info.clone()),
                            }
                        })
                        .collect();

                    // Build ArchiveEntry items for extra files.
                    // Extra files always get the wrap_in_directory prefix (if set).
                    // When ArchiveFileSpec::Detailed has dst, use it as the
                    // archive-internal name; apply per-file info permissions.
                    let extra_entries: Vec<ArchiveEntry> = extra_files
                        .iter()
                        .map(|ef| -> Result<ArchiveEntry> {
                            let base_name = if let Some(ref dst) = ef.dst {
                                dst.clone()
                            } else if ef.strip_parent {
                                // strip_parent: use only the filename, discarding
                                // any parent directory components so the file ends
                                // up at the archive root (or directly under
                                // wrap_in_directory).
                                ef.src
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "unknown".to_string())
                            } else {
                                // Use just the filename — extra files go at archive
                                // root (or under wrap_in_directory).
                                ef.src
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "unknown".to_string())
                            };
                            let raw_name = if let Some(dir) = wrap_dir {
                                PathBuf::from(dir).join(&base_name)
                            } else {
                                PathBuf::from(&base_name)
                            };
                            let archive_name = normalize_archive_path(raw_name);
                            Ok(ArchiveEntry {
                                src: ef.src.clone(),
                                archive_name,
                                info: ef
                                    .info
                                    .as_ref()
                                    .map(|i| render_file_info(i, ctx))
                                    .transpose()?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;

                    // Combine binary + extra entries and deduplicate by archive_name.
                    // Matches GoReleaser's unique() — first occurrence wins,
                    // duplicates are warned and skipped.
                    let combined: Vec<ArchiveEntry> = binary_entries
                        .into_iter()
                        .chain(extra_entries.into_iter())
                        .collect();
                    let deduped = deduplicate_entries(combined);
                    let sorted = sort_entries(deduped);
                    let all_entries: Vec<&ArchiveEntry> = sorted.iter().collect();

                    // For gz/binary formats, collect flat path refs (these formats
                    // don't support per-entry metadata)
                    let all_src_paths: Vec<PathBuf> =
                        sorted.iter().map(|e| e.src.clone()).collect();
                    let path_refs: Vec<&Path> =
                        all_src_paths.iter().map(PathBuf::as_path).collect();

                    // Determine reproducible mtime: prefer CommitTimestamp from context
                    // when any crate has reproducible: true, fall back to SOURCE_DATE_EPOCH.
                    let source_date_epoch: Option<u64> = {
                        let any_reproducible = ctx.config.crates.iter().any(|c| {
                            c.builds.as_ref().is_some_and(|builds| {
                                builds.iter().any(|b| b.reproducible.unwrap_or(false))
                            })
                        });
                        if any_reproducible {
                            ctx.template_vars()
                                .get("CommitTimestamp")
                                .and_then(|ts| ts.parse::<u64>().ok())
                        } else {
                            std::env::var("SOURCE_DATE_EPOCH")
                                .ok()
                                .and_then(|s| s.parse::<u64>().ok())
                        }
                    };

                    for format in &formats_to_produce {
                        // "none" format: skip archive creation entirely for this target
                        if format == "none" {
                            log.status(&format!(
                                "skipping archive for {crate_name}/{target} (format: none)"
                            ));
                            continue;
                        }

                        // For binary format, no extension; otherwise append format.
                        // GoReleaser uses {{ .Binary }} prefix (not {{ .ProjectName }})
                        // for binary format when no custom name_template is set.
                        let archive_filename = if format == "binary" {
                            if has_custom_name_tmpl {
                                archive_stem.clone()
                            } else {
                                ctx.render_template(default_binary_name_template())
                                    .unwrap_or_else(|_| archive_stem.clone())
                            }
                        } else {
                            format!("{archive_stem}.{format}")
                        };
                        let archive_path = dist.join(&archive_filename);

                        // Duplicate archive name detection: prevent silent overwrites
                        if archive_path.exists() {
                            bail!(
                                "archive named '{}' already exists. Check your archive name template.",
                                archive_filename
                            );
                        }

                        if dry_run {
                            log.status(&format!(
                                "(dry-run) would create {} with {} files",
                                archive_path.display(),
                                all_entries.len()
                            ));
                        } else {
                            log.status(&format!("creating {}", archive_path.display()));
                            match format.as_str() {
                                "zip" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create zip: {}", archive_path.display())
                                        })?;
                                    let mut zip = zip::ZipWriter::new(out_file);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_zip_entries(&mut zip, &owned, source_date_epoch)?;
                                    zip.finish().context("zip: finish")?;
                                }
                                "tar.gz" | "tgz" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.gz: {}", archive_path.display())
                                        })?;
                                    let enc = GzEncoder::new(out_file, Compression::best());
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.gz",
                                    )?;
                                    tar.finish().context("tar.gz: finish")?;
                                }
                                "tar.xz" | "txz" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.xz: {}", archive_path.display())
                                        })?;
                                    let enc = xz2::write::XzEncoder::new(out_file, 9);
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.xz",
                                    )?;
                                    tar.finish().context("tar.xz: finish")?;
                                }
                                "tar.zst" | "tzst" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar.zst: {}", archive_path.display())
                                        })?;
                                    let enc = zstd::Encoder::new(out_file, 3)
                                        .context("tar.zst: create zstd encoder")?;
                                    let mut tar = tar::Builder::new(enc);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar.zst",
                                    )?;
                                    let enc = tar.into_inner().context("tar.zst: finish tar")?;
                                    enc.finish().context("tar.zst: finish zstd")?;
                                }
                                "tar" => {
                                    let out_file =
                                        File::create(&archive_path).with_context(|| {
                                            format!("create tar: {}", archive_path.display())
                                        })?;
                                    let mut tar = tar::Builder::new(out_file);
                                    let owned: Vec<ArchiveEntry> = all_entries
                                        .iter()
                                        .map(|e| ArchiveEntry {
                                            src: e.src.clone(),
                                            archive_name: e.archive_name.clone(),
                                            info: e.info.clone(),
                                        })
                                        .collect();
                                    write_archive_entries(
                                        &mut tar,
                                        &owned,
                                        source_date_epoch,
                                        "tar",
                                    )?;
                                    tar.finish().context("tar: finish")?;
                                }
                                "gz" => {
                                    if path_refs.is_empty() {
                                        bail!("gz format requires at least one file");
                                    }
                                    if path_refs.len() > 1 {
                                        log.warn(&format!(
                                            "gz format only compresses a single file; {} extra files will be skipped",
                                            path_refs.len() - 1
                                        ));
                                    }
                                    create_gz(path_refs[0], &archive_path)?;
                                }
                                "binary" => copy_binary(&path_refs, &archive_path)?,
                                _ => bail!("unsupported archive format: {format}"),
                            }
                        }

                        // Update stage-scoped template vars for downstream stages
                        let tvars = ctx.template_vars_mut();
                        tvars.set("ArtifactName", &archive_filename);
                        tvars.set("ArtifactPath", &archive_path.to_string_lossy());
                        tvars.set(
                            "ArtifactExt",
                            anodize_core::template::extract_artifact_ext(&archive_filename),
                        );
                        // Set ArtifactID from archive config id (Pro addition)
                        tvars.set("ArtifactID", archive_cfg.id.as_deref().unwrap_or(""));

                        let mut metadata = HashMap::from([
                            ("format".to_string(), format.clone()),
                            ("name".to_string(), archive_stem.clone()),
                        ]);
                        // Propagate archive config id to artifact metadata for
                        // downstream stages (sign, release) to filter by archive ID
                        if let Some(ref id) = archive_cfg.id {
                            metadata.insert("id".to_string(), id.clone());
                        }
                        if is_meta {
                            metadata.insert("meta".to_string(), "true".to_string());
                        }
                        if strip_bin_dir {
                            metadata
                                .insert("strip_binary_directory".to_string(), "true".to_string());
                        }
                        if let Some(dir) = wrap_dir {
                            metadata.insert("wrap_in_directory".to_string(), dir.to_string());
                        }
                        // Store binary names in archive metadata for publisher
                        // consumption (e.g. Homebrew multi-binary install).
                        let bin_names: Vec<String> = selected_bins
                            .iter()
                            .filter_map(|b| {
                                b.metadata.get("binary").cloned().or_else(|| {
                                    b.path.file_name().map(|n| n.to_string_lossy().to_string())
                                })
                            })
                            .collect();
                        if !bin_names.is_empty() {
                            metadata.insert("extra_binaries".to_string(), bin_names.join(","));
                        }

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Archive,
                            name: String::new(),
                            path: archive_path,
                            target: Some(target.clone()),
                            crate_name: crate_name.clone(),
                            metadata,
                            size: None,
                        });
                    }
                }

                // Post-archive hooks
                if let Some(post) = archive_cfg.hooks.as_ref().and_then(|h| h.post.as_ref()) {
                    run_hooks(post, "post-archive", dry_run, &log, Some(&template_vars))?;
                }
            }
        }

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Target", "");
        ctx.template_vars_mut().set("Binary", "");
        ctx.template_vars_mut().set("ArtifactName", "");
        ctx.template_vars_mut().set("ArtifactPath", "");
        ctx.template_vars_mut().set("ArtifactExt", "");
        ctx.template_vars_mut().set("ArtifactID", "");

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use anodize_core::artifact::{Artifact, ArtifactKind};

    #[test]
    fn test_create_tar_gz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin.tar.gz");
        create_tar_gz(&[&bin_path], &archive_path, None, None, None, None).unwrap();

        assert!(archive_path.exists());
        assert!(fs::metadata(&archive_path).unwrap().len() > 0);
    }

    #[test]
    fn test_create_zip() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin.exe");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin.zip");
        create_zip(&[&bin_path], &archive_path, None, None).unwrap();

        assert!(archive_path.exists());
        assert!(fs::metadata(&archive_path).unwrap().len() > 0);
    }

    #[test]
    fn test_format_for_target() {
        assert_eq!(
            format_for_target("x86_64-unknown-linux-gnu", "tar.gz", &[]),
            "tar.gz"
        );
        assert_eq!(
            format_for_target(
                "x86_64-pc-windows-msvc",
                "tar.gz",
                &[FormatOverride {
                    os: "windows".to_string(),
                    format: Some("zip".to_string()),
                    formats: None,
                }]
            ),
            "zip"
        );
    }

    // ---------------------------------------------------------------------------
    // New tests: tar.xz
    // ---------------------------------------------------------------------------

    #[test]
    fn test_create_tar_xz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content for xz").unwrap();

        let archive_path = tmp.path().join("mybin.tar.xz");
        create_tar_xz(&[&bin_path], &archive_path, None, None, None, None).unwrap();

        assert!(archive_path.exists());
        let len = fs::metadata(&archive_path).unwrap().len();
        assert!(len > 0, "tar.xz archive should not be empty");

        // Verify we can decompress and read the tar
        let file = File::open(&archive_path).unwrap();
        let dec = xz2::read::XzDecoder::new(file);
        let mut tar = tar::Archive::new(dec);
        let entries: Vec<_> = tar.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
    }

    // ---------------------------------------------------------------------------
    // New tests: tar.zst
    // ---------------------------------------------------------------------------

    #[test]
    fn test_create_tar_zst() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content for zstd").unwrap();

        let archive_path = tmp.path().join("mybin.tar.zst");
        create_tar_zst(&[&bin_path], &archive_path, None, None, None, None).unwrap();

        assert!(archive_path.exists());
        let len = fs::metadata(&archive_path).unwrap().len();
        assert!(len > 0, "tar.zst archive should not be empty");

        // Verify we can decompress and read the tar
        let file = File::open(&archive_path).unwrap();
        let dec = zstd::Decoder::new(file).unwrap();
        let mut tar = tar::Archive::new(dec);
        let entries: Vec<_> = tar.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
    }

    // ---------------------------------------------------------------------------
    // New tests: binary format (copy)
    // ---------------------------------------------------------------------------

    #[test]
    fn test_copy_binary_single() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("myapp");
        fs::write(&src, b"actual binary bytes").unwrap();

        let dest = tmp.path().join("dist").join("myapp");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        copy_binary(&[src.as_path()], &dest).unwrap();

        assert!(dest.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"actual binary bytes");
    }

    #[test]
    fn test_copy_binary_multiple() {
        let tmp = TempDir::new().unwrap();
        let src1 = tmp.path().join("bin1");
        let src2 = tmp.path().join("bin2");
        fs::write(&src1, b"binary-1").unwrap();
        fs::write(&src2, b"binary-2").unwrap();

        let out_dir = tmp.path().join("dist");
        fs::create_dir_all(&out_dir).unwrap();
        let output = out_dir.join("placeholder");

        copy_binary(&[src1.as_path(), src2.as_path()], &output).unwrap();

        assert!(out_dir.join("bin1").exists());
        assert!(out_dir.join("bin2").exists());
        assert_eq!(fs::read(out_dir.join("bin1")).unwrap(), b"binary-1");
        assert_eq!(fs::read(out_dir.join("bin2")).unwrap(), b"binary-2");
    }

    // ---------------------------------------------------------------------------
    // New tests: glob pattern resolution
    // ---------------------------------------------------------------------------

    #[test]
    fn test_resolve_glob_patterns() {
        let tmp = TempDir::new().unwrap();
        let license = tmp.path().join("LICENSE");
        let license_mit = tmp.path().join("LICENSE-MIT");
        let readme = tmp.path().join("README.md");
        fs::write(&license, b"license").unwrap();
        fs::write(&license_mit, b"mit license").unwrap();
        fs::write(&readme, b"readme").unwrap();

        let pattern = format!("{}/*", tmp.path().display());
        let results = resolve_glob_patterns(&[pattern]).unwrap();
        assert!(
            results.len() >= 3,
            "should match at least 3 files, got {}",
            results.len()
        );

        // Test with LICENSE* glob
        let license_pattern = format!("{}/LICENSE*", tmp.path().display());
        let results = resolve_glob_patterns(&[license_pattern]).unwrap();
        assert_eq!(results.len(), 2, "LICENSE* should match 2 files");
        assert!(results.iter().any(|p| p.file_name().unwrap() == "LICENSE"));
        assert!(
            results
                .iter()
                .any(|p| p.file_name().unwrap() == "LICENSE-MIT")
        );
    }

    #[test]
    fn test_resolve_glob_patterns_literal() {
        let tmp = TempDir::new().unwrap();
        let license = tmp.path().join("LICENSE");
        fs::write(&license, b"license content").unwrap();

        // A literal (non-glob) path that exists should be returned
        let results = resolve_glob_patterns(&[license.to_string_lossy().to_string()]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], license);

        // A literal path that does not exist should be silently skipped
        let results = resolve_glob_patterns(&["/nonexistent/file".to_string()]).unwrap();
        assert!(results.is_empty());
    }

    // ---------------------------------------------------------------------------
    // New tests: wrap_in_directory
    // ---------------------------------------------------------------------------

    #[test]
    fn test_wrap_in_directory_tar_gz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        let license_path = tmp.path().join("LICENSE");
        fs::write(&bin_path, b"binary").unwrap();
        fs::write(&license_path, b"MIT").unwrap();

        let archive_path = tmp.path().join("wrapped.tar.gz");
        create_tar_gz(
            &[&bin_path, &license_path],
            &archive_path,
            None,
            Some("myapp-1.0.0"),
            None,
            None,
        )
        .unwrap();

        // Verify entries have the directory prefix
        let file = File::open(&archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(dec);
        let mut names: Vec<String> = Vec::new();
        for entry in tar.entries().unwrap() {
            let entry = entry.unwrap();
            names.push(entry.path().unwrap().to_string_lossy().to_string());
        }
        names.sort();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "myapp-1.0.0/LICENSE");
        assert_eq!(names[1], "myapp-1.0.0/mybin");
    }

    #[test]
    fn test_wrap_in_directory_zip() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin.exe");
        fs::write(&bin_path, b"binary").unwrap();

        let archive_path = tmp.path().join("wrapped.zip");
        create_zip(&[&bin_path], &archive_path, Some("myapp-1.0.0"), None).unwrap();

        // Verify entry has the directory prefix
        let file = File::open(&archive_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), 1);
        let entry = zip.by_index(0).unwrap();
        assert_eq!(entry.name(), "myapp-1.0.0/mybin.exe");
    }

    #[test]
    fn test_wrap_in_directory_tar_xz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary").unwrap();

        let archive_path = tmp.path().join("wrapped.tar.xz");
        create_tar_xz(
            &[&bin_path],
            &archive_path,
            None,
            Some("myapp-1.0.0"),
            None,
            None,
        )
        .unwrap();

        // Verify entry has the directory prefix
        let file = File::open(&archive_path).unwrap();
        let dec = xz2::read::XzDecoder::new(file);
        let mut tar = tar::Archive::new(dec);
        let entries: Vec<_> = tar.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "myapp-1.0.0/mybin");
    }

    #[test]
    fn test_wrap_in_directory_tar_zst() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary").unwrap();

        let archive_path = tmp.path().join("wrapped.tar.zst");
        create_tar_zst(
            &[&bin_path],
            &archive_path,
            None,
            Some("myapp-1.0.0"),
            None,
            None,
        )
        .unwrap();

        // Verify entry has the directory prefix
        let file = File::open(&archive_path).unwrap();
        let dec = zstd::Decoder::new(file).unwrap();
        let mut tar = tar::Archive::new(dec);
        let entries: Vec<_> = tar.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "myapp-1.0.0/mybin");
    }

    // ---------------------------------------------------------------------------
    // Config parsing test for wrap_in_directory
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_config_parses_wrap_in_directory() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - format: tar.gz
        wrap_in_directory: "myapp-{{ .Version }}"
        files:
          - LICENSE
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        match &config.crates[0].archives {
            ArchivesConfig::Configs(cfgs) => {
                assert_eq!(cfgs.len(), 1);
                assert_eq!(
                    cfgs[0].wrap_in_directory,
                    Some(anodize_core::config::WrapInDirectory::Name(
                        "myapp-{{ .Version }}".to_string()
                    ))
                );
                assert_eq!(cfgs[0].format, Some("tar.gz".to_string()));
            }
            _ => panic!("expected Configs variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Integration-style test: ArchiveStage.run
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_run() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create a fake binary
        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"fake binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    name_template: Some(
                        "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                    ),
                    format: Some("tar.gz".to_string()),
                    format_overrides: None,
                    files: None,
                    binaries: None,
                    wrap_in_directory: None,
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        // Register a Binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        // Should have registered one Archive artifact
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.exists());
        assert!(archives[0].path.to_string_lossy().ends_with(".tar.gz"));
    }

    #[test]
    fn test_archive_stage_disabled() {
        use anodize_core::config::{ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Disabled,
                ..Default::default()
            }])
            .build();

        // Register a Binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/fake/path"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        // No archives should be registered
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert!(archives.is_empty());
    }

    #[test]
    fn test_archive_stage_zip_for_windows() {
        use anodize_core::config::{
            ArchiveConfig, ArchivesConfig, Config, CrateConfig, FormatOverride,
        };
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp.exe");
        fs::write(&bin_path, b"fake windows binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                format: Some("tar.gz".to_string()),
                format_overrides: Some(vec![FormatOverride {
                    os: "windows".to_string(),
                    format: Some("zip".to_string()),
                    formats: None,
                }]),
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path.clone(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.to_string_lossy().ends_with(".zip"));
        assert!(archives[0].path.exists());
    }

    // ---------------------------------------------------------------------------
    // Integration test: ArchiveStage with tar.xz format
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_tar_xz_format() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"fake binary for xz").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                format: Some("tar.xz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.to_string_lossy().ends_with(".tar.xz"));
        assert!(archives[0].path.exists());
        assert!(fs::metadata(&archives[0].path).unwrap().len() > 0);
    }

    // ---------------------------------------------------------------------------
    // Integration test: ArchiveStage with binary format
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_binary_format() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"raw binary content").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                format: Some("binary".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        // Binary format should have no extension
        let name = archives[0].path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains(".tar"));
        assert!(!name.contains(".zip"));
        assert!(!name.contains(".gz"));
        assert!(archives[0].path.exists());
        assert_eq!(fs::read(&archives[0].path).unwrap(), b"raw binary content");
    }

    // -----------------------------------------------------------------------
    // Deep integration tests: realistic file trees, verify archive contents
    // -----------------------------------------------------------------------

    /// Helper: read all entries from a tar archive into a HashMap of name -> content.
    fn read_tar_entries<R: std::io::Read>(archive: tar::Archive<R>) -> HashMap<String, Vec<u8>> {
        let mut found_files = HashMap::new();
        let mut archive = archive;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().to_string();
            let mut content = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
            found_files.insert(name, content);
        }
        found_files
    }

    /// Helper: create a realistic file tree with a binary, LICENSE, and README.
    fn create_realistic_file_tree(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let bin = dir.join("myapp");
        let license = dir.join("LICENSE");
        let readme = dir.join("README.md");
        fs::write(
            &bin,
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03",
        )
        .unwrap();
        fs::write(
            &license,
            b"MIT License\n\nCopyright (c) 2026 Example Corp\n\nPermission is hereby granted...",
        )
        .unwrap();
        fs::write(
            &readme,
            b"# MyApp\n\nA tool for doing things.\n\n## Usage\n\n```\nmyapp --help\n```\n",
        )
        .unwrap();
        (bin, license, readme)
    }

    #[test]
    fn test_integration_tar_gz_realistic_file_tree() {
        let tmp = TempDir::new().unwrap();
        let (bin, license, readme) = create_realistic_file_tree(tmp.path());

        let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.gz");
        create_tar_gz(
            &[&bin, &license, &readme],
            &archive_path,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Open the archive and verify all files are present with correct names
        let file = File::open(&archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));

        assert_eq!(
            found_files.len(),
            3,
            "archive should contain exactly 3 files"
        );
        assert!(
            found_files.contains_key("myapp"),
            "should contain myapp binary"
        );
        assert!(
            found_files.contains_key("LICENSE"),
            "should contain LICENSE"
        );
        assert!(
            found_files.contains_key("README.md"),
            "should contain README.md"
        );

        // Verify file contents are preserved byte-for-byte
        assert_eq!(
            found_files["myapp"],
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
            "binary content should be preserved exactly"
        );
        assert!(
            found_files["LICENSE"].starts_with(b"MIT License"),
            "LICENSE content should be preserved"
        );
        assert!(
            found_files["README.md"].starts_with(b"# MyApp"),
            "README content should be preserved"
        );
    }

    #[test]
    fn test_integration_zip_realistic_file_tree() {
        let tmp = TempDir::new().unwrap();
        let (bin, license, readme) = create_realistic_file_tree(tmp.path());

        let archive_path = tmp.path().join("myapp-1.0.0-windows-amd64.zip");
        create_zip(&[&bin, &license, &readme], &archive_path, None, None).unwrap();

        // Open the zip and verify all files
        let file = File::open(&archive_path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();

        assert_eq!(zip.len(), 3, "zip should contain exactly 3 files");

        let mut found_names: Vec<String> = Vec::new();
        for i in 0..zip.len() {
            let entry = zip.by_index(i).unwrap();
            found_names.push(entry.name().to_string());
        }
        found_names.sort();
        assert_eq!(found_names, vec!["LICENSE", "README.md", "myapp"]);

        // Verify binary content is preserved
        {
            let mut bin_entry = zip.by_name("myapp").unwrap();
            let mut bin_content = Vec::new();
            std::io::Read::read_to_end(&mut bin_entry, &mut bin_content).unwrap();
            assert_eq!(
                bin_content,
                b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
                "binary content in zip should be preserved exactly"
            );
        }

        // Verify LICENSE content is preserved
        {
            let mut lic_entry = zip.by_name("LICENSE").unwrap();
            let mut lic_content = Vec::new();
            std::io::Read::read_to_end(&mut lic_entry, &mut lic_content).unwrap();
            assert!(lic_content.starts_with(b"MIT License"));
        }
    }

    #[test]
    fn test_integration_tar_xz_realistic_file_tree() {
        let tmp = TempDir::new().unwrap();
        let (bin, license, readme) = create_realistic_file_tree(tmp.path());

        let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.xz");
        create_tar_xz(
            &[&bin, &license, &readme],
            &archive_path,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Open the archive and verify all files
        let file = File::open(&archive_path).unwrap();
        let dec = xz2::read::XzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));

        assert_eq!(
            found_files.len(),
            3,
            "tar.xz should contain exactly 3 files"
        );
        assert!(found_files.contains_key("myapp"));
        assert!(found_files.contains_key("LICENSE"));
        assert!(found_files.contains_key("README.md"));

        // Verify binary content
        assert_eq!(
            found_files["myapp"],
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
            "binary content in tar.xz should be preserved exactly"
        );

        // Verify text content
        let readme_str = String::from_utf8(found_files["README.md"].clone()).unwrap();
        assert!(
            readme_str.contains("## Usage"),
            "README structure should be intact"
        );
        assert!(
            readme_str.contains("myapp --help"),
            "README content should be preserved"
        );
    }

    #[test]
    fn test_integration_tar_gz_with_wrap_dir_contents_verified() {
        let tmp = TempDir::new().unwrap();
        let (bin, license, readme) = create_realistic_file_tree(tmp.path());

        let archive_path = tmp.path().join("myapp-1.0.0.tar.gz");
        create_tar_gz(
            &[&bin, &license, &readme],
            &archive_path,
            None,
            Some("myapp-1.0.0"),
            None,
            None,
        )
        .unwrap();

        let file = File::open(&archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));

        // All entries should be prefixed with wrap directory
        assert_eq!(found_files.len(), 3);
        assert!(found_files.contains_key("myapp-1.0.0/myapp"));
        assert!(found_files.contains_key("myapp-1.0.0/LICENSE"));
        assert!(found_files.contains_key("myapp-1.0.0/README.md"));

        // Contents still preserved after wrapping
        assert_eq!(
            found_files["myapp-1.0.0/myapp"],
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec()
        );
    }

    // -----------------------------------------------------------------------
    // Integration test: tar.zst with realistic file tree
    // -----------------------------------------------------------------------

    #[test]
    fn test_integration_tar_zst_realistic_file_tree() {
        let tmp = TempDir::new().unwrap();
        let (bin, license, readme) = create_realistic_file_tree(tmp.path());

        let archive_path = tmp.path().join("myapp-1.0.0-linux-amd64.tar.zst");
        create_tar_zst(
            &[&bin, &license, &readme],
            &archive_path,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Open the archive and verify all files
        let file = File::open(&archive_path).unwrap();
        let dec = zstd::Decoder::new(file).unwrap();
        let found_files = read_tar_entries(tar::Archive::new(dec));

        assert_eq!(
            found_files.len(),
            3,
            "tar.zst should contain exactly 3 files"
        );
        assert!(
            found_files.contains_key("myapp"),
            "should contain myapp binary"
        );
        assert!(
            found_files.contains_key("LICENSE"),
            "should contain LICENSE"
        );
        assert!(
            found_files.contains_key("README.md"),
            "should contain README.md"
        );

        // Verify binary content is preserved byte-for-byte
        assert_eq!(
            found_files["myapp"],
            b"\x7fELF fake binary content with some bytes: \x00\x01\x02\x03".to_vec(),
            "binary content in tar.zst should be preserved exactly"
        );

        // Verify text content
        assert!(
            found_files["LICENSE"].starts_with(b"MIT License"),
            "LICENSE content should be preserved"
        );
        let readme_str = String::from_utf8(found_files["README.md"].clone()).unwrap();
        assert!(
            readme_str.contains("## Usage"),
            "README structure should be intact"
        );
        assert!(
            readme_str.contains("myapp --help"),
            "README content should be preserved"
        );
    }

    // -- TestContextBuilder integration test: verify stage-scoped vars --

    #[test]
    fn test_archive_stage_scoped_vars_not_preset() {
        use anodize_core::test_helpers::TestContextBuilder;

        let ctx = TestContextBuilder::new()
            .project_name("archive-test")
            .tag("v1.0.0")
            .build();

        // Os and Arch are stage-scoped — they should NOT be set by the builder.
        // The archive stage sets them per-target during execution.
        assert_eq!(ctx.template_vars().get("Os"), None);
        assert_eq!(ctx.template_vars().get("Arch"), None);

        // But project-level vars should be present
        assert_eq!(
            ctx.template_vars().get("ProjectName"),
            Some(&"archive-test".to_string())
        );
        assert_eq!(
            ctx.template_vars().get("Version"),
            Some(&"1.0.0".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_for_target_multiple_overrides() {
        // Multiple OS overrides: windows->zip AND darwin->tar.gz
        let overrides = vec![
            FormatOverride {
                os: "windows".to_string(),
                format: Some("zip".to_string()),
                formats: None,
            },
            FormatOverride {
                os: "darwin".to_string(),
                format: Some("tar.gz".to_string()),
                formats: None,
            },
        ];
        // Default is tar.xz but windows should get zip
        assert_eq!(
            format_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides),
            "zip"
        );
        // darwin should get tar.gz
        assert_eq!(
            format_for_target("aarch64-apple-darwin", "tar.xz", &overrides),
            "tar.gz"
        );
        // Linux falls through to default
        assert_eq!(
            format_for_target("x86_64-unknown-linux-gnu", "tar.xz", &overrides),
            "tar.xz"
        );
    }

    #[test]
    fn test_archive_stage_multiple_binaries_per_archive() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create two fake binaries for the same target
        let bin1 = tmp.path().join("myapp");
        let bin2 = tmp.path().join("myhelper");
        fs::write(&bin1, b"binary 1").unwrap();
        fs::write(&bin2, b"binary 2").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}".to_string(),
                ),
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None, // Include all binaries
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Register two binary artifacts for the same target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin1.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin2.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myhelper".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        // Should create one archive containing both binaries
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert!(archives[0].path.exists());

        // Verify both binaries are in the archive
        let file = File::open(&archives[0].path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));
        assert_eq!(found_files.len(), 2, "archive should contain both binaries");
        assert!(found_files.contains_key("myapp"));
        assert!(found_files.contains_key("myhelper"));
    }

    #[test]
    fn test_archive_stage_default_config_inheritance() {
        use anodize_core::config::{
            ArchiveConfig, ArchivesConfig, Config, CrateConfig, DefaultArchiveConfig, Defaults,
            FormatOverride,
        };
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp.exe");
        fs::write(&bin, b"fake windows binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            // Use default archive config (no format_overrides set) — should inherit global
            archives: ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];
        // Global defaults: format_overrides windows -> zip
        config.defaults = Some(Defaults {
            archives: Some(DefaultArchiveConfig {
                format: Some("tar.gz".to_string()),
                format_overrides: Some(vec![FormatOverride {
                    os: "windows".to_string(),
                    format: Some("zip".to_string()),
                    formats: None,
                }]),
            }),
            ..Default::default()
        });

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin.clone(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        // Should have inherited global format_override: windows -> zip
        assert!(
            archives[0].path.to_string_lossy().ends_with(".zip"),
            "windows archive should use zip from global defaults, got: {}",
            archives[0].path.display()
        );
    }

    #[test]
    fn test_archive_stage_name_template_renders_all_variables() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"fake binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}".to_string(),
                ),
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: None,
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "2.5.0");
        ctx.template_vars_mut().set("Tag", "v2.5.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin.clone(),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);

        let name = archives[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "myapp_2.5.0_darwin_arm64.tar.gz");
    }

    #[test]
    fn test_archive_stage_files_included_alongside_binaries() {
        use anodize_core::config::{
            ArchiveConfig, ArchiveFileSpec, ArchivesConfig, Config, CrateConfig,
        };
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        let license = tmp.path().join("LICENSE");
        let readme = tmp.path().join("README.md");
        fs::write(&bin, b"binary content").unwrap();
        fs::write(&license, b"MIT License").unwrap();
        fs::write(&readme, b"# MyApp").unwrap();

        let license_pattern = tmp.path().join("LICENSE").to_string_lossy().to_string();
        let readme_pattern = tmp.path().join("README.md").to_string_lossy().to_string();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some("myapp-1.0.0-linux-amd64".to_string()),
                format: Some("tar.gz".to_string()),
                format_overrides: None,
                files: Some(vec![
                    ArchiveFileSpec::Glob(license_pattern),
                    ArchiveFileSpec::Glob(readme_pattern),
                ]),
                binaries: None,
                wrap_in_directory: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);

        // Verify all 3 files are in the archive
        let file = File::open(&archives[0].path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));
        assert_eq!(
            found_files.len(),
            3,
            "archive should contain binary + 2 extra files"
        );
        assert!(found_files.contains_key("myapp"));
        assert!(found_files.contains_key("LICENSE"));
        assert!(found_files.contains_key("README.md"));
    }

    #[test]
    fn test_archive_registers_correct_metadata() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("zip".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        // Verify the artifact metadata contains format and name
        assert_eq!(archives[0].metadata.get("format"), Some(&"zip".to_string()));
        assert!(archives[0].metadata.contains_key("name"));
        // Verify it's registered as an Archive artifact for the right crate
        assert_eq!(archives[0].crate_name, "myapp");
        assert_eq!(archives[0].kind, ArtifactKind::Archive);
        // Target should be preserved
        assert_eq!(
            archives[0].target.as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
    }

    #[test]
    fn test_archive_stage_wrap_in_directory_renders_template() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some("myapp-linux-amd64".to_string()),
                format: Some("tar.gz".to_string()),
                wrap_in_directory: Some(anodize_core::config::WrapInDirectory::Name(
                    "{{ .ProjectName }}-{{ .Version }}".to_string(),
                )),
                files: None,
                format_overrides: None,
                binaries: None,
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "3.0.0");
        ctx.template_vars_mut().set("Tag", "v3.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);

        // Verify that the wrap directory was rendered from the template
        let file = File::open(&archives[0].path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found = read_tar_entries(tar::Archive::new(dec));
        assert!(
            found.contains_key("myapp-3.0.0/myapp"),
            "wrap directory should use rendered template, got keys: {:?}",
            found.keys().collect::<Vec<_>>()
        );
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_binary_artifact_errors_with_path() {
        use anodize_core::config::{ArchiveConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: anodize_core::config::ArchivesConfig::Configs(vec![ArchiveConfig::default()]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        // Register a binary artifact that doesn't exist on disk
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/nonexistent/path/to/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("binary".to_string(), "myapp".to_string());
                m
            },
            size: None,
        });

        let result = ArchiveStage.run(&mut ctx);
        assert!(result.is_err(), "archive with missing binary should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("binary artifact missing") || err.contains("/nonexistent/path/to/myapp"),
            "error should mention the missing binary path, got: {err}"
        );
    }

    #[test]
    fn test_empty_file_list_creates_empty_tar_gz() {
        let tmp = TempDir::new().unwrap();
        let archive_path = tmp.path().join("empty.tar.gz");

        // Create an archive with empty file list
        let result = create_tar_gz(&[], &archive_path, None, None, None, None);
        assert!(
            result.is_ok(),
            "creating archive with empty file list should succeed"
        );
        assert!(archive_path.exists(), "archive file should be created");
    }

    #[test]
    fn test_empty_file_list_creates_empty_zip() {
        let tmp = TempDir::new().unwrap();
        let archive_path = tmp.path().join("empty.zip");

        let result = create_zip(&[], &archive_path, None, None);
        assert!(
            result.is_ok(),
            "creating zip with empty file list should succeed"
        );
        assert!(archive_path.exists(), "zip file should be created");
    }

    #[test]
    fn test_copy_binary_source_missing_errors_with_path() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let output = tmp.path().join("output");

        let result = copy_binary(&[missing.as_path()], &output);
        assert!(
            result.is_err(),
            "copy_binary with missing source should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist") || err.contains("does-not-exist"),
            "error should mention the missing file, got: {err}"
        );
    }

    #[test]
    fn test_archive_unsupported_format_returns_error() {
        // Unknown archive formats should produce a clear error.
        use anodize_core::config::{ArchiveConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();

        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"fake binary").unwrap();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: anodize_core::config::ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("unsupported_format".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("binary".to_string(), "mybin".to_string());
                m
            },
            size: None,
        });

        let result = ArchiveStage.run(&mut ctx);
        assert!(result.is_err(), "unsupported format should return an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unsupported archive format"),
            "error should mention 'unsupported archive format', got: {err}"
        );
    }

    // ---- Task 5E: reproducible archive mtime tests ----

    #[test]
    fn test_create_tar_gz_with_fixed_mtime() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin-reproducible.tar.gz");
        let fixed_mtime: u64 = 1_700_000_000;
        create_tar_gz(
            &[&bin_path],
            &archive_path,
            None,
            None,
            Some(fixed_mtime),
            None,
        )
        .unwrap();

        assert!(archive_path.exists());

        // Verify the stored mtime matches the fixed timestamp
        let file = File::open(&archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(dec);
        let mut entries = tar.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.header().mtime().unwrap(),
            fixed_mtime,
            "tar.gz entry mtime should match SOURCE_DATE_EPOCH"
        );
    }

    #[test]
    fn test_create_tar_xz_with_fixed_mtime() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin-reproducible.tar.xz");
        let fixed_mtime: u64 = 1_700_000_000;
        create_tar_xz(
            &[&bin_path],
            &archive_path,
            None,
            None,
            Some(fixed_mtime),
            None,
        )
        .unwrap();

        assert!(archive_path.exists());

        let file = File::open(&archive_path).unwrap();
        let dec = xz2::read::XzDecoder::new(file);
        let mut tar = tar::Archive::new(dec);
        let mut entries = tar.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.header().mtime().unwrap(),
            fixed_mtime,
            "tar.xz entry mtime should match SOURCE_DATE_EPOCH"
        );
    }

    #[test]
    fn test_create_tar_zst_with_fixed_mtime() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("mybin-reproducible.tar.zst");
        let fixed_mtime: u64 = 1_700_000_000;
        create_tar_zst(
            &[&bin_path],
            &archive_path,
            None,
            None,
            Some(fixed_mtime),
            None,
        )
        .unwrap();

        assert!(archive_path.exists());

        let file = File::open(&archive_path).unwrap();
        let dec = zstd::Decoder::new(file).unwrap();
        let mut tar = tar::Archive::new(dec);
        let mut entries = tar.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.header().mtime().unwrap(),
            fixed_mtime,
            "tar.zst entry mtime should match SOURCE_DATE_EPOCH"
        );
    }

    #[test]
    fn test_reproducible_archive_is_deterministic() {
        // Two archives created with the same content and fixed mtime must be byte-identical
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"deterministic binary content").unwrap();

        let fixed_mtime: u64 = 1_700_000_000;
        let archive1 = tmp.path().join("archive1.tar.gz");
        let archive2 = tmp.path().join("archive2.tar.gz");

        create_tar_gz(&[&bin_path], &archive1, None, None, Some(fixed_mtime), None).unwrap();
        create_tar_gz(&[&bin_path], &archive2, None, None, Some(fixed_mtime), None).unwrap();

        let bytes1 = fs::read(&archive1).unwrap();
        let bytes2 = fs::read(&archive2).unwrap();
        assert_eq!(
            bytes1, bytes2,
            "archives with same content and fixed mtime should be byte-identical"
        );
    }

    // -----------------------------------------------------------------------
    // ids filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_archive_ids_filter_only_matching_builds() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create two fake binaries: one with id "linux-build", one with id "windows-build"
        let linux_bin = tmp.path().join("myapp-linux");
        let windows_bin = tmp.path().join("myapp-windows");
        fs::write(&linux_bin, b"linux binary").unwrap();
        fs::write(&windows_bin, b"windows binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist.clone())
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    ids: Some(vec!["linux-build".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        // Register binaries with different build IDs
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: linux_bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "linux-build".to_string()),
            ]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: windows_bin,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "windows-build".to_string()),
            ]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(
            archives.len(),
            1,
            "only one archive should be created (linux-build only)"
        );
        assert!(
            archives[0].target.as_deref().unwrap().contains("linux"),
            "archive should be for the linux target"
        );
    }

    #[test]
    fn test_archive_ids_filter_excludes_all_when_no_match() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    ids: Some(vec!["nonexistent-id".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "some-other-id".to_string()),
            ]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert!(
            archives.is_empty(),
            "no archives should be created when ids filter matches nothing"
        );
    }

    #[test]
    fn test_archive_ids_filter_none_includes_all() {
        // When ids is None, all binaries should be included (backward compat)
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let linux_bin = tmp.path().join("myapp-linux");
        let win_bin = tmp.path().join("myapp-win");
        fs::write(&linux_bin, b"linux binary").unwrap();
        fs::write(&win_bin, b"windows binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    // ids is None (default) — all binaries should be included
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: linux_bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "linux-build".to_string()),
            ]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: win_bin,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "windows-build".to_string()),
            ]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(
            archives.len(),
            2,
            "both targets should produce archives when ids is None"
        );
    }

    // -----------------------------------------------------------------------
    // id metadata tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_archive_id_metadata_propagated() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    id: Some("linux-archive".to_string()),
                    format: Some("tar.gz".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("id"),
            Some(&"linux-archive".to_string()),
            "archive artifact should have the config id in metadata"
        );
    }

    #[test]
    fn test_archive_id_metadata_absent_when_not_set() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    // id is None (default)
                    format: Some("tar.gz".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("id"),
            None,
            "archive artifact should not have id in metadata when config id is None"
        );
    }

    // -----------------------------------------------------------------------
    // formats (plural) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_archive_formats_plural_produces_multiple_archives() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary content").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    formats: Some(vec!["tar.gz".to_string(), "zip".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(
            archives.len(),
            2,
            "should produce one archive per format in formats list"
        );

        let mut formats: Vec<String> = archives
            .iter()
            .map(|a| a.metadata.get("format").unwrap().clone())
            .collect();
        formats.sort();
        assert_eq!(formats, vec!["tar.gz", "zip"]);

        // Both archives should exist on disk
        for a in &archives {
            assert!(
                a.path.exists(),
                "archive should exist: {}",
                a.path.display()
            );
        }

        // Verify file extensions
        let paths: Vec<String> = archives
            .iter()
            .map(|a| a.path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.ends_with(".tar.gz")),
            "should have a tar.gz archive"
        );
        assert!(
            paths.iter().any(|p| p.ends_with(".zip")),
            "should have a zip archive"
        );
    }

    #[test]
    fn test_archive_formats_plural_ignores_singular_format() {
        // When formats (plural) is set, singular format should be ignored
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary content").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.xz".to_string()), // should be ignored
                    formats: Some(vec!["tar.gz".to_string(), "zip".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 2);

        let mut formats: Vec<String> = archives
            .iter()
            .map(|a| a.metadata.get("format").unwrap().clone())
            .collect();
        formats.sort();
        assert_eq!(
            formats,
            vec!["tar.gz", "zip"],
            "should use formats (plural), not singular format"
        );
    }

    #[test]
    fn test_archive_formats_empty_falls_back_to_singular() {
        // When formats is Some but empty, fall back to singular format
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary content").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.xz".to_string()),
                    formats: Some(vec![]), // empty — should fall back to singular
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("format").unwrap(),
            "tar.xz",
            "empty formats should fall back to singular format"
        );
    }

    #[test]
    fn test_archive_singular_format_still_works_when_formats_absent() {
        // Backward compat: when formats is None, singular format works as before
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_path = tmp.path().join("myapp");
        fs::write(&bin_path, b"binary content").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("zip".to_string()),
                    // formats is None (default)
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("format").unwrap(),
            "zip",
            "singular format should work when formats is absent"
        );
        assert!(archives[0].path.exists());
        assert!(archives[0].path.to_string_lossy().ends_with(".zip"));
    }

    // ---------------------------------------------------------------------------
    // Uncompressed tar archive
    // ---------------------------------------------------------------------------

    #[test]
    fn test_create_tar_uncompressed() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content for tar").unwrap();

        let archive_path = tmp.path().join("mybin.tar");
        create_tar(&[&bin_path], &archive_path, None, None, None, None).unwrap();

        assert!(archive_path.exists());
        let len = fs::metadata(&archive_path).unwrap().len();
        assert!(len > 0, "uncompressed tar archive should not be empty");

        // Verify we can read the tar directly (no decompression needed)
        let file = File::open(&archive_path).unwrap();
        let mut tar = tar::Archive::new(file);
        let entries: Vec<_> = tar.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "mybin");
    }

    // ---------------------------------------------------------------------------
    // Format alias tests: tgz, txz, tzst, tar via stage
    // ---------------------------------------------------------------------------

    #[test]
    fn test_archive_stage_tgz_alias() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("tgz".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].metadata.get("format"), Some(&"tgz".to_string()));
        assert!(archives[0].path.exists(), "tgz archive file should exist");
    }

    #[test]
    fn test_archive_stage_txz_alias() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("txz".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].metadata.get("format"), Some(&"txz".to_string()));
        assert!(archives[0].path.exists(), "txz archive file should exist");
    }

    #[test]
    fn test_archive_stage_tzst_alias() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("tzst".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("format"),
            Some(&"tzst".to_string())
        );
        assert!(archives[0].path.exists(), "tzst archive file should exist");
    }

    #[test]
    fn test_archive_stage_uncompressed_tar() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("tar".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].metadata.get("format"), Some(&"tar".to_string()));
        assert!(archives[0].path.exists(), "tar archive file should exist");
    }

    #[test]
    fn test_archive_stage_unknown_format_errors() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("rar".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        let result = ArchiveStage.run(&mut ctx);
        assert!(result.is_err(), "unknown format should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unsupported archive format") && err.contains("rar"),
            "error should mention the unsupported format, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Task 3: Config parsing tests for new parity features
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_archive_file_spec_glob() {
        use anodize_core::config::{ArchiveFileSpec, Config};
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - files:
          - LICENSE*
          - README.md
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
            let files = cfgs[0].files.as_ref().unwrap();
            assert_eq!(files.len(), 2);
            assert_eq!(files[0], "LICENSE*");
            assert_eq!(files[1], "README.md");
            // Verify it deserialized as Glob variant
            assert!(matches!(&files[0], ArchiveFileSpec::Glob(_)));
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn test_config_parse_archive_file_spec_detailed() {
        use anodize_core::config::{ArchiveFileSpec, Config};
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - files:
          - src: "LICENSE*"
            dst: "licenses/"
            info:
              owner: root
              group: root
              mode: "0644"
          - src: "completions/*"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
            let files = cfgs[0].files.as_ref().unwrap();
            assert_eq!(files.len(), 2);
            match &files[0] {
                ArchiveFileSpec::Detailed { src, dst, info, .. } => {
                    assert_eq!(src, "LICENSE*");
                    assert_eq!(dst.as_deref(), Some("licenses/"));
                    let info = info.as_ref().unwrap();
                    assert_eq!(info.owner.as_deref(), Some("root"));
                    assert_eq!(info.group.as_deref(), Some("root"));
                    assert_eq!(info.mode.as_deref(), Some("0644"));
                }
                _ => panic!("expected Detailed variant for first entry"),
            }
            match &files[1] {
                ArchiveFileSpec::Detailed { src, dst, info, .. } => {
                    assert_eq!(src, "completions/*");
                    assert!(dst.is_none());
                    assert!(info.is_none());
                }
                _ => panic!("expected Detailed variant for second entry"),
            }
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn test_config_parse_format_override_formats_plural() {
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - format_overrides:
          - os: windows
            formats:
              - zip
              - tar.gz
          - os: darwin
            format: tar.xz
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
            let overrides = cfgs[0].format_overrides.as_ref().unwrap();
            assert_eq!(overrides.len(), 2);
            // First override: windows with plural formats
            assert_eq!(overrides[0].os, "windows");
            assert!(overrides[0].format.is_none());
            let fmts = overrides[0].formats.as_ref().unwrap();
            assert_eq!(fmts, &["zip", "tar.gz"]);
            // Second override: darwin with singular format
            assert_eq!(overrides[1].os, "darwin");
            assert_eq!(overrides[1].format, Some("tar.xz".to_string()));
            assert!(overrides[1].formats.is_none());
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn test_config_parse_meta_builds_info_strip_allow() {
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - meta: true
        strip_binary_directory: true
        allow_different_binary_count: true
        builds_info:
          owner: root
          group: root
          mode: "0755"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
            assert_eq!(cfgs[0].meta, Some(true));
            assert_eq!(cfgs[0].strip_binary_directory, Some(true));
            assert_eq!(cfgs[0].allow_different_binary_count, Some(true));
            let bi = cfgs[0].builds_info.as_ref().unwrap();
            assert_eq!(bi.owner.as_deref(), Some("root"));
            assert_eq!(bi.group.as_deref(), Some("root"));
            assert_eq!(bi.mode.as_deref(), Some("0755"));
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn test_config_parse_archive_hooks() {
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - hooks:
          pre:
            - echo pre-archive
          post:
            - echo post-archive
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        if let ArchivesConfig::Configs(cfgs) = &config.crates[0].archives {
            let hooks = cfgs[0].hooks.as_ref().unwrap();
            let pre = hooks.pre.as_ref().unwrap();
            assert_eq!(pre.len(), 1);
            assert_eq!(pre[0], "echo pre-archive");
            let post = hooks.post.as_ref().unwrap();
            assert_eq!(post.len(), 1);
            assert_eq!(post[0], "echo post-archive");
        } else {
            panic!("expected Configs variant");
        }
    }

    // -----------------------------------------------------------------------
    // gz format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_gz() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content for gz").unwrap();

        let archive_path = tmp.path().join("mybin.gz");
        create_gz(&bin_path, &archive_path).unwrap();

        assert!(archive_path.exists());
        let len = fs::metadata(&archive_path).unwrap().len();
        assert!(len > 0, "gz archive should not be empty");

        // Verify we can decompress and get the original content
        let compressed = fs::read(&archive_path).unwrap();
        let mut dec = flate2::read::GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut decompressed).unwrap();
        assert_eq!(decompressed, b"binary content for gz");
    }

    #[test]
    fn test_create_gz_nonexistent_fails() {
        let tmp = TempDir::new().unwrap();
        let archive_path = tmp.path().join("empty.gz");
        let nonexistent = tmp.path().join("does_not_exist");
        let result = create_gz(&nonexistent, &archive_path);
        assert!(result.is_err(), "gz with nonexistent file should fail");
    }

    #[test]
    fn test_archive_stage_gz_format() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary content").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                format: Some("gz".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].metadata.get("format"), Some(&"gz".to_string()));
        assert!(archives[0].path.exists(), "gz archive file should exist");
        assert!(
            archives[0].path.to_string_lossy().ends_with(".gz"),
            "gz archive should have .gz extension"
        );
    }

    // -----------------------------------------------------------------------
    // meta archive test
    // -----------------------------------------------------------------------

    #[test]
    fn test_archive_stage_meta_no_binaries() {
        use anodize_core::config::{
            ArchiveConfig, ArchiveFileSpec, ArchivesConfig, Config, CrateConfig,
        };
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        // Create extra files but no binary
        let license = tmp.path().join("LICENSE");
        fs::write(&license, b"MIT License").unwrap();
        let license_path = license.to_string_lossy().to_string();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                meta: Some(true),
                format: Some("tar.gz".to_string()),
                files: Some(vec![ArchiveFileSpec::Glob(license_path)]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist;
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // No binary artifacts registered at all

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1, "meta archive should be created");
        assert_eq!(
            archives[0].metadata.get("meta"),
            Some(&"true".to_string()),
            "should be marked as meta"
        );
        assert!(archives[0].path.exists());

        // Verify the archive only contains the LICENSE file, no binaries
        let file = File::open(&archives[0].path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found = read_tar_entries(tar::Archive::new(dec));
        assert_eq!(
            found.len(),
            1,
            "meta archive should contain only the extra file"
        );
        assert!(found.contains_key("LICENSE"));
    }

    // -----------------------------------------------------------------------
    // format_overrides.formats plural test
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_override_formats_plural() {
        // FormatOverride with plural formats should produce multiple formats
        let overrides = vec![FormatOverride {
            os: "windows".to_string(),
            format: None,
            formats: Some(vec!["zip".to_string(), "tar.gz".to_string()]),
        }];
        let result = formats_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides);
        assert_eq!(result, vec!["zip", "tar.gz"]);
    }

    #[test]
    fn test_format_override_formats_plural_priority_over_singular() {
        // When both format and formats are set, formats takes priority
        let overrides = vec![FormatOverride {
            os: "windows".to_string(),
            format: Some("tar.gz".to_string()),
            formats: Some(vec!["zip".to_string()]),
        }];
        let result = formats_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides);
        assert_eq!(result, vec!["zip"]);
    }

    #[test]
    fn test_format_override_formats_empty_falls_back_to_singular() {
        // Empty formats falls back to singular format
        let overrides = vec![FormatOverride {
            os: "windows".to_string(),
            format: Some("zip".to_string()),
            formats: Some(vec![]),
        }];
        let result = formats_for_target("x86_64-pc-windows-msvc", "tar.xz", &overrides);
        assert_eq!(result, vec!["zip"]);
    }

    // -----------------------------------------------------------------------
    // allow_different_binary_count test (warning only)
    // -----------------------------------------------------------------------

    #[test]
    fn test_allow_different_binary_count_default_errors_on_mismatch() {
        // GoReleaser errors (not warns) when binary counts differ and
        // allow_different_binary_count is false (default).
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let linux_bin = tmp.path().join("myapp-linux");
        let win_bin1 = tmp.path().join("myapp-win1");
        let win_bin2 = tmp.path().join("myapp-win2");
        fs::write(&linux_bin, b"linux binary").unwrap();
        fs::write(&win_bin1, b"windows binary 1").unwrap();
        fs::write(&win_bin2, b"windows binary 2").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    // allow_different_binary_count is None (default false) - should error
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        // Different binary counts per target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: linux_bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: win_bin1,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: win_bin2,
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "helper".to_string())]),
            size: None,
        });

        // Should error when binary counts differ (matching GoReleaser behavior)
        let result = ArchiveStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "different binary counts should error, not warn"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("binary counts differ"),
            "error should mention binary count mismatch, got: {err}"
        );
        assert!(
            err.contains("allow_different_binary_count"),
            "error should suggest the fix, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // strip_binary_directory metadata test
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_binary_directory_metadata() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, CrateConfig};
        use anodize_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        let bin = tmp.path().join("myapp");
        fs::write(&bin, b"binary").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    strip_binary_directory: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("binary".to_string(), "myapp".to_string())]),
            size: None,
        });

        ArchiveStage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);
        assert_eq!(
            archives[0].metadata.get("strip_binary_directory"),
            Some(&"true".to_string()),
        );
    }

    // -----------------------------------------------------------------------
    // resolve_file_specs tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_file_specs_glob() {
        use anodize_core::config::ArchiveFileSpec;

        let tmp = TempDir::new().unwrap();
        let license = tmp.path().join("LICENSE");
        fs::write(&license, b"MIT").unwrap();

        let specs = vec![ArchiveFileSpec::Glob(license.to_string_lossy().to_string())];
        let resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].src, license);
        assert!(resolved[0].dst.is_none());
        assert!(resolved[0].info.is_none());
    }

    #[test]
    fn test_resolve_file_specs_detailed() {
        use anodize_core::config::{ArchiveFileInfo, ArchiveFileSpec};

        let tmp = TempDir::new().unwrap();
        let license = tmp.path().join("LICENSE");
        fs::write(&license, b"MIT").unwrap();

        let specs = vec![ArchiveFileSpec::Detailed {
            src: license.to_string_lossy().to_string(),
            dst: Some("licenses/".to_string()),
            info: Some(ArchiveFileInfo {
                owner: Some("root".to_string()),
                group: Some("root".to_string()),
                mode: Some("0644".to_string()),
                mtime: None,
            }),
            strip_parent: None,
        }];
        let resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].src, license);
        // With LCP logic: single file, LCP is the file path itself, so
        // prefix_dir = parent dir, rel = filename, dst = "licenses/LICENSE"
        assert_eq!(resolved[0].dst.as_deref(), Some("licenses/LICENSE"));
        let info = resolved[0].info.as_ref().unwrap();
        assert_eq!(info.owner.as_deref(), Some("root"));
        assert_eq!(info.mode.as_deref(), Some("0644"));
    }

    // -----------------------------------------------------------------------
    // longest_common_prefix tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_lcp_empty() {
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn test_lcp_single() {
        let strs = vec!["/home/user/docs/README.md".to_string()];
        assert_eq!(longest_common_prefix(&strs), "/home/user/docs/README.md");
    }

    #[test]
    fn test_lcp_multiple_common() {
        let strs = vec![
            "/home/user/docs/README.md".to_string(),
            "/home/user/docs/guide/intro.md".to_string(),
            "/home/user/docs/guide/advanced.md".to_string(),
        ];
        assert_eq!(longest_common_prefix(&strs), "/home/user/docs/");
    }

    #[test]
    fn test_lcp_no_common_prefix() {
        let strs = vec![
            "/usr/local/bin/foo".to_string(),
            "/home/user/bar".to_string(),
        ];
        assert_eq!(longest_common_prefix(&strs), "/");
    }

    #[test]
    fn test_lcp_identical_strings() {
        let strs = vec![
            "/home/user/file.txt".to_string(),
            "/home/user/file.txt".to_string(),
        ];
        assert_eq!(longest_common_prefix(&strs), "/home/user/file.txt");
    }

    // -----------------------------------------------------------------------
    // resolve_file_specs with dst — directory preservation via LCP
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_file_specs_dst_preserves_directory_structure() {
        use anodize_core::config::ArchiveFileSpec;

        let tmp = TempDir::new().unwrap();
        let docs_dir = tmp.path().join("docs");
        let guide_dir = docs_dir.join("guide");
        fs::create_dir_all(&guide_dir).unwrap();
        fs::write(docs_dir.join("README.md"), b"readme").unwrap();
        fs::write(guide_dir.join("intro.md"), b"intro").unwrap();

        let glob_pattern = format!("{}/**/*.md", docs_dir.display());
        let specs = vec![ArchiveFileSpec::Detailed {
            src: glob_pattern,
            dst: Some("mydocs".to_string()),
            info: None,
            strip_parent: None,
        }];

        let mut resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 2);

        // Sort by dst for deterministic assertions
        resolved.sort_by(|a, b| a.dst.cmp(&b.dst));

        // The LCP of "/tmp/.../docs/README.md" and "/tmp/.../docs/guide/intro.md"
        // is "/tmp/.../docs/" which IS an existing directory, so prefix_dir = docs_dir.
        // Relative paths: "README.md" and "guide/intro.md"
        // Destinations: "mydocs/README.md" and "mydocs/guide/intro.md"
        assert_eq!(resolved[0].dst.as_deref(), Some("mydocs/README.md"));
        assert_eq!(resolved[1].dst.as_deref(), Some("mydocs/guide/intro.md"));
    }

    #[test]
    fn test_resolve_file_specs_dst_with_strip_parent_ignores_lcp() {
        use anodize_core::config::ArchiveFileSpec;

        let tmp = TempDir::new().unwrap();
        let docs_dir = tmp.path().join("docs");
        let guide_dir = docs_dir.join("guide");
        fs::create_dir_all(&guide_dir).unwrap();
        fs::write(docs_dir.join("README.md"), b"readme").unwrap();
        fs::write(guide_dir.join("intro.md"), b"intro").unwrap();

        let glob_pattern = format!("{}/**/*.md", docs_dir.display());
        let specs = vec![ArchiveFileSpec::Detailed {
            src: glob_pattern,
            dst: Some("mydocs".to_string()),
            info: None,
            strip_parent: Some(true),
        }];

        let resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 2);

        // When strip_parent is true, dst is passed through as-is (no LCP logic)
        for r in &resolved {
            assert_eq!(r.dst.as_deref(), Some("mydocs"));
            assert!(r.strip_parent);
        }
    }

    #[test]
    fn test_resolve_file_specs_literal_src_with_dst_preserves_filename() {
        use anodize_core::config::ArchiveFileSpec;

        let tmp = TempDir::new().unwrap();
        let license = tmp.path().join("LICENSE");
        fs::write(&license, b"MIT License").unwrap();

        // Literal (non-glob) src with a dst directory — our LCP logic should
        // produce "licenses/LICENSE" rather than renaming the file to "licenses".
        // This is an intentional divergence from GoReleaser, which would rename
        // the file.
        let specs = vec![ArchiveFileSpec::Detailed {
            src: license.to_string_lossy().to_string(),
            dst: Some("licenses".to_string()),
            info: None,
            strip_parent: None,
        }];

        let resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].src, license);
        assert_eq!(resolved[0].dst.as_deref(), Some("licenses/LICENSE"));
    }

    #[test]
    fn test_resolve_file_specs_dst_partial_filename_lcp_fallback() {
        use anodize_core::config::ArchiveFileSpec;

        let tmp = TempDir::new().unwrap();
        // Two files whose names share a prefix — the LCP of their full paths
        // will be something like "/tmp/.../file_" which is NOT a directory.
        // The code should fall back to the parent directory so both files
        // appear under dst with just their filenames.
        let alpha = tmp.path().join("file_alpha.txt");
        let beta = tmp.path().join("file_beta.txt");
        fs::write(&alpha, b"alpha").unwrap();
        fs::write(&beta, b"beta").unwrap();

        let glob_pattern = format!("{}/file_*.txt", tmp.path().display());
        let specs = vec![ArchiveFileSpec::Detailed {
            src: glob_pattern,
            dst: Some("output".to_string()),
            info: None,
            strip_parent: None,
        }];

        let mut resolved = resolve_file_specs(&specs).unwrap();
        assert_eq!(resolved.len(), 2);

        // Sort for deterministic assertions
        resolved.sort_by(|a, b| a.dst.cmp(&b.dst));

        // LCP is "/tmp/.../file_" which is not a directory, so prefix_dir
        // falls back to the parent dir ("/tmp/.../"). Relative paths are
        // "file_alpha.txt" and "file_beta.txt".
        assert_eq!(resolved[0].dst.as_deref(), Some("output/file_alpha.txt"));
        assert_eq!(resolved[1].dst.as_deref(), Some("output/file_beta.txt"));
    }

    // -----------------------------------------------------------------------
    // builds_info: verify permissions apply to tar entries
    // -----------------------------------------------------------------------

    #[test]
    fn test_append_tar_entry_with_file_info_mode() {
        use anodize_core::config::ArchiveFileInfo;

        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("test.tar");
        let out_file = File::create(&archive_path).unwrap();
        let mut tar = tar::Builder::new(out_file);

        let info = ArchiveFileInfo {
            mode: Some("0755".to_string()),
            owner: Some("deploy".to_string()),
            group: Some("staff".to_string()),
            mtime: None,
        };

        append_tar_entry(&mut tar, &bin_path, Path::new("mybin"), None, Some(&info)).unwrap();
        tar.finish().unwrap();

        // Read back the archive and verify permissions
        let file = File::open(&archive_path).unwrap();
        let mut tar = tar::Archive::new(file);
        let mut entries = tar.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        let header = entry.header();

        // Mode should be 0o755
        assert_eq!(header.mode().unwrap() & 0o777, 0o755, "mode should be 0755");
        assert_eq!(
            header.username().unwrap().unwrap(),
            "deploy",
            "owner should be 'deploy'"
        );
        assert_eq!(
            header.groupname().unwrap().unwrap(),
            "staff",
            "group should be 'staff'"
        );
    }

    #[test]
    fn test_write_tar_entries_with_file_info() {
        use anodize_core::config::ArchiveFileInfo;

        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("mybin");
        fs::write(&bin_path, b"binary content").unwrap();

        let archive_path = tmp.path().join("test.tar");
        let out_file = File::create(&archive_path).unwrap();
        let mut tar = tar::Builder::new(out_file);

        let info = ArchiveFileInfo {
            mode: Some("0755".to_string()),
            owner: None,
            group: None,
            mtime: None,
        };

        write_tar_entries(
            &mut tar,
            &[bin_path.as_path()],
            None,
            None,
            None,
            Some(&info),
            "test",
        )
        .unwrap();
        tar.finish().unwrap();

        // Read back and verify
        let file = File::open(&archive_path).unwrap();
        let mut tar = tar::Archive::new(file);
        let mut entries = tar.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.header().mode().unwrap() & 0o777,
            0o755,
            "write_tar_entries should apply file_info mode"
        );
    }

    // ---------------------------------------------------------------------------
    // deduplicate_entries
    // ---------------------------------------------------------------------------

    #[test]
    fn test_deduplicate_entries_keeps_first_skips_later() {
        let entries = vec![
            ArchiveEntry {
                src: PathBuf::from("/src/a/mybin"),
                archive_name: PathBuf::from("bin/mybin"),
                info: None,
            },
            ArchiveEntry {
                src: PathBuf::from("/src/b/mybin"),
                archive_name: PathBuf::from("bin/mybin"),
                info: None,
            },
            ArchiveEntry {
                src: PathBuf::from("LICENSE"),
                archive_name: PathBuf::from("LICENSE"),
                info: None,
            },
        ];

        let deduped = deduplicate_entries(entries);
        assert_eq!(deduped.len(), 2, "duplicate should be removed");
        assert_eq!(deduped[0].src, PathBuf::from("/src/a/mybin"));
        assert_eq!(deduped[0].archive_name, PathBuf::from("bin/mybin"));
        assert_eq!(deduped[1].archive_name, PathBuf::from("LICENSE"));
    }

    // ---------------------------------------------------------------------------
    // sort_entries
    // ---------------------------------------------------------------------------

    #[test]
    fn test_sort_entries_by_archive_name() {
        let entries = vec![
            ArchiveEntry {
                src: PathBuf::from("z.txt"),
                archive_name: PathBuf::from("c.txt"),
                info: None,
            },
            ArchiveEntry {
                src: PathBuf::from("a.txt"),
                archive_name: PathBuf::from("a.txt"),
                info: None,
            },
            ArchiveEntry {
                src: PathBuf::from("m.txt"),
                archive_name: PathBuf::from("b.txt"),
                info: None,
            },
        ];

        let sorted = sort_entries(entries);
        let names: Vec<String> = sorted
            .iter()
            .map(|e| e.archive_name.to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
    }

    // ---------------------------------------------------------------------------
    // render_file_info
    // ---------------------------------------------------------------------------

    #[test]
    fn test_render_file_info_templates() {
        use anodize_core::config::ArchiveFileInfo;
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("ProjectName", "myapp");

        let info = ArchiveFileInfo {
            owner: Some("{{ .ProjectName }}".to_string()),
            group: Some("staff".to_string()),
            mode: Some("0755".to_string()),
            mtime: Some("{{ .Version }}".to_string()),
        };

        let rendered = render_file_info(&info, &ctx).unwrap();
        assert_eq!(rendered.owner.as_deref(), Some("myapp"));
        assert_eq!(rendered.group.as_deref(), Some("staff"));
        assert_eq!(rendered.mode.as_deref(), Some("0755")); // mode not rendered
        assert_eq!(rendered.mtime.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn test_archive_stage_binaries_filter() {
        use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dist = tmp.path().join("dist");

        let bin_a = tmp.path().join("app-a");
        let bin_b = tmp.path().join("app-b");
        fs::write(&bin_a, b"binary-a").unwrap();
        fs::write(&bin_b, b"binary-b").unwrap();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some("filtered-archive".to_string()),
                format: Some("tar.gz".to_string()),
                binaries: Some(vec!["app-a".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        for (name, path) in [("app-a", &bin_a), ("app-b", &bin_b)] {
            let mut metadata = HashMap::new();
            metadata.insert("binary".to_string(), name.to_string());
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: path.clone(),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata,
                size: None,
            });
        }

        let stage = ArchiveStage;
        stage.run(&mut ctx).unwrap();

        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1);

        // Open archive and verify only app-a is inside, not app-b
        let archive_path = &archives[0].path;
        let file = File::open(archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let found_files = read_tar_entries(tar::Archive::new(dec));

        assert!(
            found_files.keys().any(|n| n.contains("app-a")),
            "should contain app-a: {:?}",
            found_files.keys().collect::<Vec<_>>()
        );
        assert!(
            !found_files.keys().any(|n| n.contains("app-b")),
            "should NOT contain app-b: {:?}",
            found_files.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_default_name_template_includes_amd64_suffix() {
        let tmpl = default_name_template();
        assert!(
            tmpl.contains("Amd64"),
            "default name template should contain Amd64 conditional: {tmpl}"
        );
        let bin_tmpl = default_binary_name_template();
        assert!(
            bin_tmpl.contains("Amd64"),
            "default binary name template should contain Amd64 conditional: {bin_tmpl}"
        );
    }

    #[test]
    fn test_default_template_renders_amd64_v2_suffix() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "amd64");
        ctx.template_vars_mut().set("Amd64", "v2");

        let result = ctx.render_template(default_name_template()).unwrap();
        assert_eq!(result, "myapp_1.0.0_linux_amd64v2");
    }

    #[test]
    fn test_default_template_omits_amd64_v1_suffix() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "amd64");
        ctx.template_vars_mut().set("Amd64", "v1");

        let result = ctx.render_template(default_name_template()).unwrap();
        assert_eq!(result, "myapp_1.0.0_linux_amd64");
    }
}
