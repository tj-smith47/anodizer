//! Archive entry pipeline — the `ArchiveEntry` value type plus the
//! `dedup → sort → write` helpers shared by tar and zip codecs. Lives in
//! its own module so the deterministic-archive invariants (unique
//! archive_name, alphabetical order, reproducible mtime) are visible
//! without scrolling through codec writers.

use std::collections::HashMap;
use std::fs;
use std::io::Write as IoWrite;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::{archive_log, formats};

/// An entry to add to an archive, carrying source path, archive-internal name,
/// and optional per-file info (permissions/owner/group).
#[derive(Clone)]
pub(crate) struct ArchiveEntry {
    /// Source file path on disk.
    pub src: PathBuf,
    /// Name inside the archive (may differ from src filename due to dst override).
    pub archive_name: PathBuf,
    /// Per-file info overrides (mode/owner/group/mtime).
    pub info: Option<anodizer_core::config::ArchiveFileInfo>,
}

/// Deduplicate archive entries by `archive_name`. When a duplicate destination
/// path is found, emit a warning to stderr and keep only the first occurrence.
/// Deduplicate entries, first occurrence wins.
pub(crate) fn deduplicate_entries(entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(first_src) = seen.get(&entry.archive_name) {
            archive_log().warn(&format!(
                "file '{}' already exists in archive as '{}' — '{}' will be ignored",
                entry.archive_name.display(),
                first_src.display(),
                entry.src.display(),
            ));
        } else {
            seen.insert(entry.archive_name.clone(), entry.src.clone());
            result.push(entry);
        }
    }
    result
}

/// Sort archive entries by `archive_name` to ensure deterministic ordering.
/// This sort is essential
/// for reproducible archives.
pub(crate) fn sort_entries(mut entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    entries.sort_by(|a, b| a.archive_name.cmp(&b.archive_name));
    entries
}

/// Write a list of `ArchiveEntry` items into a tar builder, applying per-entry
/// file info and an optional global mtime.
pub(crate) fn write_archive_entries<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    entries: &[ArchiveEntry],
    mtime: Option<u64>,
    label: &str,
) -> Result<()> {
    for entry in entries {
        if !entry.src.exists() {
            archive_log().warn(&format!(
                "{label}: '{}' no longer exists — omitted from the archive",
                entry.src.display()
            ));
            continue;
        }
        formats::append_tar_entry(
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
pub(crate) fn write_zip_entries<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    entries: &[ArchiveEntry],
    mtime: Option<u64>,
) -> Result<()> {
    let mut base_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    if let Some(ts) = mtime
        && let Some(zip_dt) = unix_timestamp_to_zip_datetime(ts)
    {
        base_options = base_options.last_modified_time(zip_dt);
    }

    for entry in entries {
        if !entry.src.exists() {
            archive_log().warn(&format!(
                "zip archive: '{}' no longer exists — omitted from the archive",
                entry.src.display()
            ));
            continue;
        }
        let mut options = base_options;
        if let Some(ref info) = entry.info
            && let Some(mode) = info.mode
        {
            options = options.unix_permissions(mode.value());
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
