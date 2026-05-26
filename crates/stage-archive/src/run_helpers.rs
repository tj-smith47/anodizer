//! Helpers extracted from `run.rs` to reduce that file's god-function size
//! while keeping behavior identical.

use std::fs::File;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;

use anodizer_core::config::{ArchiveConfig, VALID_ARCHIVE_FORMATS};
use anodizer_core::log::StageLogger;

use crate::entries::{ArchiveEntry, write_archive_entries, write_zip_entries};
use crate::formats::{copy_binary, create_gz, create_xz};

pub(crate) fn validate_archive_configs(
    work: &[(String, std::path::PathBuf, Vec<ArchiveConfig>)],
    log: &StageLogger,
) -> Result<()> {
    for (_crate_name, _crate_dir, archive_cfgs) in work {
        for cfg in archive_cfgs {
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
                    if ov.os.is_empty() {
                        log.warn("format_override has empty goos/os value");
                    }
                    if ov.formats.as_ref().is_none_or(|f| f.is_empty()) {
                        log.warn("format_override has empty formats value");
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
    Ok(())
}

fn entries_to_owned(all_entries: &[&ArchiveEntry]) -> Vec<ArchiveEntry> {
    all_entries
        .iter()
        .map(|e| ArchiveEntry {
            src: e.src.clone(),
            archive_name: e.archive_name.clone(),
            info: e.info.clone(),
        })
        .collect()
}

pub(crate) fn write_archive_in_format(
    format: &str,
    archive_path: &Path,
    all_entries: &[&ArchiveEntry],
    path_refs: &[&Path],
    source_date_epoch: Option<u64>,
    log: &StageLogger,
) -> Result<()> {
    match format {
        "zip" => {
            let out_file = File::create(archive_path)
                .with_context(|| format!("create zip: {}", archive_path.display()))?;
            let mut zip = zip::ZipWriter::new(out_file);
            write_zip_entries(&mut zip, &entries_to_owned(all_entries), source_date_epoch)?;
            zip.finish().context("zip: finish")?;
        }
        "tar.gz" | "tgz" => {
            let out_file = File::create(archive_path)
                .with_context(|| format!("create tar.gz: {}", archive_path.display()))?;
            let enc = GzEncoder::new(out_file, Compression::best());
            let mut tar = tar::Builder::new(enc);
            write_archive_entries(
                &mut tar,
                &entries_to_owned(all_entries),
                source_date_epoch,
                "tar.gz",
            )?;
            tar.finish().context("tar.gz: finish")?;
        }
        "tar.xz" | "txz" => {
            let out_file = File::create(archive_path)
                .with_context(|| format!("create tar.xz: {}", archive_path.display()))?;
            let enc = xz2::write::XzEncoder::new(out_file, 9);
            let mut tar = tar::Builder::new(enc);
            write_archive_entries(
                &mut tar,
                &entries_to_owned(all_entries),
                source_date_epoch,
                "tar.xz",
            )?;
            tar.finish().context("tar.xz: finish")?;
        }
        "tar.zst" | "tzst" => {
            let out_file = File::create(archive_path)
                .with_context(|| format!("create tar.zst: {}", archive_path.display()))?;
            let enc = zstd::Encoder::new(out_file, 3).context("tar.zst: create zstd encoder")?;
            let mut tar = tar::Builder::new(enc);
            write_archive_entries(
                &mut tar,
                &entries_to_owned(all_entries),
                source_date_epoch,
                "tar.zst",
            )?;
            let enc = tar.into_inner().context("tar.zst: finish tar")?;
            enc.finish().context("tar.zst: finish zstd")?;
        }
        "tar" => {
            let out_file = File::create(archive_path)
                .with_context(|| format!("create tar: {}", archive_path.display()))?;
            let mut tar = tar::Builder::new(out_file);
            write_archive_entries(
                &mut tar,
                &entries_to_owned(all_entries),
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
            create_gz(path_refs[0], archive_path)?;
        }
        "xz" => {
            if path_refs.is_empty() {
                bail!("xz format requires exactly one file");
            }
            if path_refs.len() > 1 {
                bail!(
                    "xz: failed to add {}, only one file can be archived in xz format",
                    path_refs[1].display()
                );
            }
            create_xz(path_refs[0], archive_path)?;
        }
        "binary" => copy_binary(path_refs, archive_path)?,
        other => bail!("unsupported archive format: {other}"),
    }
    Ok(())
}

pub(crate) fn resolve_archive_mtime(ctx: &anodizer_core::context::Context) -> Option<u64> {
    let any_reproducible = ctx.config.crates.iter().any(|c| {
        c.builds
            .as_ref()
            .is_some_and(|builds| builds.iter().any(|b| b.reproducible.unwrap_or(false)))
    });
    let commit_ts = ctx
        .template_vars()
        .get("CommitTimestamp")
        .and_then(|ts| ts.parse::<u64>().ok());
    if any_reproducible {
        commit_ts
    } else {
        ctx.env_var("SOURCE_DATE_EPOCH")
            .and_then(|s| s.parse::<u64>().ok())
            .or(commit_ts)
    }
}

pub(crate) fn clear_archive_template_vars(ctx: &mut anodizer_core::context::Context) {
    let tvars = ctx.template_vars_mut();
    tvars.set("Os", "");
    tvars.set("Arch", "");
    tvars.set("Target", "");
    tvars.set("Binary", "");
    tvars.set("ArtifactName", "");
    tvars.set("ArtifactPath", "");
    tvars.set("ArtifactExt", "");
    tvars.set("ArtifactID", "");
}
