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
) -> Result<()> {
    if let Some(mode) = info.mode {
        header.set_mode(mode.value());
    }
    // A configured owner/group that overflows the 32-byte ustar field (or
    // carries an interior NUL) must fail loudly rather than be silently dropped
    // to the default — the user asked for an ownership that cannot be honored.
    if let Some(ref owner) = info.owner {
        header
            .set_username(owner)
            .with_context(|| format!("archive: owner '{owner}' invalid for tar header"))?;
    }
    if let Some(ref group) = info.group {
        header
            .set_groupname(group)
            .with_context(|| format!("archive: group '{group}' invalid for tar header"))?;
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
    Ok(())
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
            apply_file_info_to_header(&mut header, info)?;
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

/// Flush the finished archive to the OS and surface any write-back error.
///
/// A format writer's `finish()` only guarantees the bytes reached the file
/// descriptor. Dropping the `File` afterwards discards the close-time error a
/// full disk or an NFS write-back reports, so a truncated archive would be
/// published as a success.
pub(crate) fn finish_archive_file(file: File, label: &str, output: &Path) -> Result<()> {
    file.sync_all()
        .with_context(|| format!("{label}: failed to close archive file {}", output.display()))
}

// ---------------------------------------------------------------------------
// tar / zip container open + close
// ---------------------------------------------------------------------------

/// A tar container's codec, closed down to the `File` underneath it.
///
/// The uncompressed `tar` container implements this as the identity, so one
/// open/close sequence serves every codec.
pub(crate) trait TarCodec: IoWrite + Sized {
    /// Codec name in the close-error context (`tar.gz: finish gzip`).
    const CODEC: &'static str;

    fn finish_to_file(self) -> Result<File>;
}

impl TarCodec for GzEncoder<File> {
    const CODEC: &'static str = "gzip";

    fn finish_to_file(self) -> Result<File> {
        Ok(self.finish()?)
    }
}

impl TarCodec for xz2::write::XzEncoder<File> {
    const CODEC: &'static str = "xz";

    fn finish_to_file(self) -> Result<File> {
        Ok(self.finish()?)
    }
}

impl TarCodec for zstd::Encoder<'static, File> {
    const CODEC: &'static str = "zstd";

    fn finish_to_file(self) -> Result<File> {
        Ok(self.finish()?)
    }
}

impl TarCodec for File {
    const CODEC: &'static str = "tar";

    fn finish_to_file(self) -> Result<File> {
        Ok(self)
    }
}

/// What goes inside one tar container, written into the builder the container
/// opened. Generic over the codec so the same entry list can be written into
/// any of them.
pub(crate) trait TarEntries {
    fn write_into<W: IoWrite>(self, tar: &mut tar::Builder<W>, label: &str) -> Result<()>;
}

/// Create `output` in `format` (`tar.gz` / `tar.xz` / `tar.zst` / `tar`),
/// write `entries` into it, then close the tar, the codec and the file.
///
/// Compression levels live here: gzip `best`, xz preset 9, zstd 3.
pub(crate) fn write_tar_archive<E: TarEntries>(
    format: &str,
    output: &Path,
    entries: E,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create {format}: {}", output.display()))?;
    match format {
        "tar.gz" => close_tar(
            format,
            output,
            GzEncoder::new(out_file, Compression::best()),
            entries,
        ),
        "tar.xz" => close_tar(
            format,
            output,
            xz2::write::XzEncoder::new(out_file, 9),
            entries,
        ),
        // Level 3 is zstd's default, matching the Go zstd library used by
        // the archiver dependency. Level 19 (near-max) was much slower with
        // marginal size improvement for release artifacts.
        "tar.zst" => close_tar(
            format,
            output,
            zstd::Encoder::new(out_file, 3).context("tar.zst: create zstd encoder")?,
            entries,
        ),
        "tar" => close_tar(format, output, out_file, entries),
        other => bail!("unsupported tar format: {other}"),
    }
}

/// Write the entries, then unwind builder → codec → file, reporting each
/// close error instead of dropping it: a codec dropped mid-flush or a file
/// dropped before `sync_all` publishes a truncated archive as a success.
fn close_tar<W: TarCodec, E: TarEntries>(
    label: &str,
    output: &Path,
    codec: W,
    entries: E,
) -> Result<()> {
    let mut tar = tar::Builder::new(codec);
    entries.write_into(&mut tar, label)?;
    tar.finish().with_context(|| format!("{label}: finish"))?;
    let codec = tar
        .into_inner()
        .with_context(|| format!("{label}: finish tar"))?;
    let out_file = codec
        .finish_to_file()
        .with_context(|| format!("{label}: finish {}", W::CODEC))?;
    finish_archive_file(out_file, label, output)
}

/// Create `output` as a zip, write entries into it through `entries`, then
/// close the zip and the file.
pub(crate) fn write_zip_archive(
    output: &Path,
    entries: impl FnOnce(&mut zip::ZipWriter<File>) -> Result<()>,
) -> Result<()> {
    let out_file =
        File::create(output).with_context(|| format!("create zip: {}", output.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    entries(&mut zip)?;
    let out_file = zip.finish().context("zip: finish")?;
    finish_archive_file(out_file, "zip", output)
}

/// The `files` list the `create_tar*` entry points write into a container.
pub(crate) struct FileEntries<'a> {
    pub(crate) files: &'a [&'a Path],
    pub(crate) base_dir: Option<&'a Path>,
    pub(crate) wrap_dir: Option<&'a str>,
    pub(crate) mtime: Option<u64>,
    pub(crate) file_info: Option<&'a anodizer_core::config::ArchiveFileInfo>,
}

impl TarEntries for FileEntries<'_> {
    fn write_into<W: IoWrite>(self, tar: &mut tar::Builder<W>, label: &str) -> Result<()> {
        write_tar_entries(
            tar,
            self.files,
            self.base_dir,
            self.wrap_dir,
            self.mtime,
            self.file_info,
            label,
        )
    }
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
    write_tar_archive(
        "tar.gz",
        output,
        FileEntries {
            files,
            base_dir,
            wrap_dir,
            mtime,
            file_info,
        },
    )
}

/// Create a tar.xz archive containing the given files.
///
/// Compression preset is fixed at level 9 (xz2's `XzEncoder::new(_, 9)`),
/// matching liblzma's `LZMA_PRESET_EXTREME`-adjacent profile. The
/// resulting dictionary size is 64 MiB — strictly larger than
/// a `DictCap` of 16 MiB — so anodizer's tar.xz is at least as
/// compressible, and decoders that handle the conventional output decode
/// anodizer's too. There is no per-archive `compression_level`
/// for xz, so neither does anodizer.
pub fn create_tar_xz(
    files: &[&Path],
    output: &Path,
    base_dir: Option<&Path>,
    wrap_dir: Option<&str>,
    mtime: Option<u64>,
    file_info: Option<&anodizer_core::config::ArchiveFileInfo>,
) -> Result<()> {
    write_tar_archive(
        "tar.xz",
        output,
        FileEntries {
            files,
            base_dir,
            wrap_dir,
            mtime,
            file_info,
        },
    )
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
    write_tar_archive(
        "tar.zst",
        output,
        FileEntries {
            files,
            base_dir,
            wrap_dir,
            mtime,
            file_info,
        },
    )
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
    write_tar_archive(
        "tar",
        output,
        FileEntries {
            files,
            base_dir,
            wrap_dir,
            mtime,
            file_info,
        },
    )
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
    let out_file = enc.finish().context("gz: finish")?;
    finish_archive_file(out_file, "gz", output)
}

/// Create a standalone .xz file from a single input file.
/// Unlike tar.xz, this compresses one file directly with xz (xz cannot hold
/// multiple files without tar). The xz container is single-file, so callers
/// must dispatch with exactly one source. Error mirrors upstream's
/// `xz: failed to add %s, only one file can be archived in xz format`.
///
/// Preset 9 dictionary size (~64 MiB) is strictly larger than the conventional
/// hand-picked `DictCap: 16 MiB`, so the resulting `.xz` is no less
/// compressible than upstream and decodes interchangeably.
pub fn create_xz(file: &Path, output: &Path) -> Result<()> {
    if !file.exists() {
        bail!("xz: source file does not exist: {}", file.display());
    }
    let out_file =
        File::create(output).with_context(|| format!("create xz: {}", output.display()))?;
    let mut enc = xz2::write::XzEncoder::new(out_file, 9);
    let data = fs::read(file).with_context(|| format!("xz: read {}", file.display()))?;
    enc.write_all(&data).context("xz: write compressed data")?;
    let out_file = enc.finish().context("xz: finish")?;
    finish_archive_file(out_file, "xz", output)
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
    let mut options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    if let Some(info) = file_info
        && let Some(mode) = info.mode
    {
        options = options.unix_permissions(mode.value());
    }

    write_zip_archive(output, |zip| {
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
        Ok(())
    })
}

/// Copy one binary directly to `output` (the `binary` archive format — no
/// archiving). `output` carries the rendered `name_template` for this binary,
/// so each build target's copy lands at its own path.
pub fn copy_binary(src: &Path, output: &Path) -> Result<()> {
    if !src.exists() {
        anyhow::bail!("binary: source does not exist: {}", src.display());
    }
    let ctx = || format!("binary: copy {} → {}", src.display(), output.display());
    let mut reader = File::open(src).with_context(ctx)?;
    let mut writer = File::create(output).with_context(ctx)?;
    std::io::copy(&mut reader, &mut writer).with_context(ctx)?;
    // `fs::copy` carried the source mode across; an explicit create does not,
    // and a `binary` artifact that lost its executable bit is unusable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = reader.metadata().with_context(ctx)?.permissions().mode();
        writer
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .with_context(ctx)?;
    }
    finish_archive_file(writer, "binary", output)
}

/// Normalize path separators: backslashes to forward slashes for archive entries.
/// Replaces `\\` with `/` in archive paths.
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
