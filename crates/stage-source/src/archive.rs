//! Source archive generation via `git archive`, with optional extra-files staging.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::config::SourceFileEntry;

pub(crate) struct SourceArchiveInputs<'a> {
    pub(crate) dist: &'a Path,
    pub(crate) format: &'a str,
    pub(crate) name: &'a str,
    pub(crate) prefix: &'a str,
    pub(crate) extra_files: &'a [SourceFileEntry],
    pub(crate) repo_root: &'a Path,
    pub(crate) commit: &'a str,
    pub(crate) log: &'a anodizer_core::log::StageLogger,
    pub(crate) strict: bool,
    /// Pre-resolved `SOURCE_DATE_EPOCH` mtime. Caller resolves via
    /// `ctx.env_var` so archive creation stays free of `std::env` calls.
    pub(crate) sde_mtime: Option<u64>,
}

/// Convert a `SOURCE_DATE_EPOCH` seconds value into a zip `DateTime` for an
/// entry's `last_modified_time`.
///
/// The zip (MS-DOS) timestamp format spans 1980..=2107 at 2-second resolution,
/// so the epoch is clamped into that window and the UTC calendar fields are
/// fed to [`zip::DateTime::from_date_and_time`]. Pinning the time explicitly
/// keeps zip source archives byte-stable regardless of whether the `zip`
/// crate's `time` feature is enabled (with it on, `SimpleFileOptions::default`
/// would otherwise stamp the wall-clock).
fn zip_datetime_from_epoch(epoch_secs: u64) -> Option<zip::DateTime> {
    let (y, mo, d, h, mi, s) = anodizer_core::sde::zip_datetime_fields(epoch_secs)?;
    zip::DateTime::from_date_and_time(y, mo, d, h, mi, s).ok()
}

/// The destination an extra file takes inside the archive prefix, in whichever
/// format the stage is writing.
///
/// One rule for every format: a `dst` renames literally, a `dst` under
/// `strip_parent` names the directory the basename ends up in, and an absent
/// `dst` preserves the file's layout relative to the repository root. An
/// absolute path outside the repository has no relative form, so its basename
/// is the only destination that does not write the machine's own directory
/// tree into the archive.
fn extra_dest_rel(entry: &SourceFileEntry, repo_root: &Path) -> String {
    let src = Path::new(&entry.src);
    let basename = || {
        src.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.src.clone())
    };
    match (&entry.dst, entry.strip_parent.unwrap_or(false)) {
        (Some(dst), true) => format!("{}/{}", dst.trim_end_matches('/'), basename()),
        (Some(dst), false) => dst.replace('\\', "/"),
        (None, true) => basename(),
        (None, false) => src
            .strip_prefix(repo_root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| {
                if src.is_absolute() {
                    basename()
                } else {
                    src.strip_prefix("./")
                        .unwrap_or(src)
                        .to_string_lossy()
                        .replace('\\', "/")
                }
            }),
    }
}

/// Extra files are placed under the prefix directory
/// by creating a temporary staging directory and using `tar --append` to
/// insert them into the archive after creation.
pub(crate) fn create_source_archive(inputs: &SourceArchiveInputs<'_>) -> Result<PathBuf> {
    let SourceArchiveInputs {
        dist,
        format,
        name,
        prefix,
        extra_files,
        repo_root,
        commit,
        log,
        strict,
        sde_mtime,
    } = *inputs;
    let (git_format, extension) = match format {
        "tar.gz" | "tgz" => ("tar.gz", "tar.gz"),
        "tar" => ("tar", "tar"),
        "zip" => ("zip", "zip"),
        _ => bail!(
            "source: unsupported archive format '{}' (use tar.gz, tgz, tar, or zip)",
            format
        ),
    };

    let filename = format!("{}.{}", name, extension);
    let output_path = dist.join(&filename);

    // For tar-based formats with extra files, create as uncompressed tar first,
    // append extra files under the prefix, then compress if needed.
    let needs_post_append = !extra_files.is_empty() && git_format != "zip";
    let initial_format = if needs_post_append { "tar" } else { git_format };
    let initial_path = if needs_post_append {
        dist.join(format!("{}.tar.tmp", name))
    } else {
        output_path.clone()
    };

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd.arg("archive").arg("--format").arg(initial_format);

    // Only pass --prefix when prefix is non-empty; omit it when unset.
    // Pass the user's prefix verbatim — do not force-append `/`.
    // Users who want directory semantics supply the trailing slash themselves.
    if !prefix.is_empty() {
        cmd.arg(format!("--prefix={}", prefix));
    }

    cmd.arg("--output").arg(&initial_path);

    // For zip format with extra files, the base archive is created first via
    // git archive, then extra files are appended under the prefix using the zip crate.
    // (--add-file puts files at root, which is wrong when prefix is set.)

    cmd.arg(commit);

    log.debug(&format!(
        "running git archive --format {} {}--output {} {}",
        initial_format,
        if prefix.is_empty() {
            String::new()
        } else {
            format!("--prefix={} ", prefix)
        },
        initial_path.display(),
        commit,
    ));
    let output = cmd
        .output()
        .context("source: failed to run 'git archive'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("source: git archive failed: {}", stderr.trim());
    }

    // Append extra files to zip under the prefix (matching tar behavior)
    if git_format == "zip" && !extra_files.is_empty() {
        use std::io::{Read as _, Write as _};

        let zip_data = std::fs::read(&output_path).context("source: read zip for appending")?;
        let reader = std::io::Cursor::new(&zip_data);
        let mut archive = zip::ZipArchive::new(reader).context("source: open zip archive")?;

        // Pin every rewritten entry's last-modified stamp to SOURCE_DATE_EPOCH
        // (same source the tar path uses) when reproducibility is in effect, so
        // the zip is byte-stable across runs and immune to the `zip` crate's
        // `time` feature defaulting to wall-clock.
        let sde_zip_time = sde_mtime.and_then(zip_datetime_from_epoch);

        // Track the compression method observed in the source archive's
        // entries. The copy preserves the original
        // archive's compression on round-trip; anodizer must do the same
        // for appended extras so a Stored (uncompressed) source archive
        // does not silently grow Deflated members.
        //
        // Heuristic: pick the method of the first non-directory entry in
        // the source archive. `git archive` produces consistent
        // compression across all entries within a single zip, so the
        // first-entry method is representative.
        let mut source_compression: Option<zip::CompressionMethod> = None;

        let mut out_buf = Vec::new();
        {
            let writer = std::io::Cursor::new(&mut out_buf);
            let mut zip_writer = zip::ZipWriter::new(writer);

            // Names the git archive already carries, so an extra file that
            // names one of them is not appended a second time.
            let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Copy existing entries
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).context("source: read zip entry")?;
                let entry_method = entry.compression();
                if source_compression.is_none() && !entry.is_dir() {
                    source_compression = Some(entry_method);
                }
                let mut options =
                    zip::write::SimpleFileOptions::default().compression_method(entry_method);
                // An executable committed to the repo must stay executable
                // after the rewrite; the default options reset every entry to
                // 0o644.
                if let Some(mode) = entry.unix_mode() {
                    options = options.unix_permissions(mode);
                }
                if let Some(t) = sde_zip_time {
                    options = options.last_modified_time(t);
                }
                let entry_name = entry.name().to_string();
                written.insert(entry_name.clone());
                zip_writer
                    .start_file(entry_name, options)
                    .context("source: start zip entry")?;
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .context("source: read zip entry data")?;
                zip_writer
                    .write_all(&data)
                    .context("source: write zip entry")?;
            }

            // Pick the compression method for appended extras: prefer the
            // source archive's method, fall back to Deflated when the
            // source is empty (no entries observed).
            let extras_method = source_compression.unwrap_or(zip::CompressionMethod::Deflated);

            // Iterate in archive-path order so two runs against the same
            // input set produce byte-identical zips. The glob expansion that
            // built `extra_files` walks the filesystem in inode order which
            // differs between fresh worktrees (matches the tar path's sort).
            let mut sorted_extras: Vec<&SourceFileEntry> = extra_files.iter().collect();
            sorted_extras.sort_by(|a, b| a.src.cmp(&b.src));

            // Append extra files under prefix
            for file_entry in sorted_extras {
                let src = std::path::Path::new(&file_entry.src);
                let dest_rel = extra_dest_rel(file_entry, repo_root);

                let archive_path = if prefix.is_empty() {
                    dest_rel
                } else {
                    // The prefix carries its own trailing slash whenever it
                    // names a directory, and a doubled separator is a distinct
                    // entry name from the one `git archive` wrote.
                    format!("{}/{}", prefix.trim_end_matches('/'), dest_rel)
                };

                if !src.exists() {
                    if strict {
                        bail!(
                            "source: extra file '{}' not found (strict mode)",
                            file_entry.src
                        );
                    }
                    log.warn(&format!(
                        "skipped extra file '{}' — not found",
                        file_entry.src
                    ));
                    continue;
                }

                let file_data = std::fs::read(src)
                    .with_context(|| format!("source: read extra file '{}'", file_entry.src))?;

                let mut options =
                    zip::write::SimpleFileOptions::default().compression_method(extras_method);
                if let Some(t) = sde_zip_time {
                    options = options.last_modified_time(t);
                }
                if !written.insert(archive_path.clone()) {
                    log.warn(&format!(
                        "skipped extra file '{}' — '{}' is already in the archive",
                        file_entry.src, archive_path
                    ));
                    continue;
                }
                zip_writer
                    .start_file(&archive_path, options)
                    .context("source: start zip extra file entry")?;
                zip_writer
                    .write_all(&file_data)
                    .context("source: write zip extra file")?;
            }

            zip_writer.finish().context("source: finish zip")?;
        }

        std::fs::write(&output_path, &out_buf).context("source: write updated zip")?;
    }

    // Append extra files using the Rust tar crate for per-file metadata control
    if needs_post_append {
        use std::io::Read as _;

        // Read the git-archive tar into memory
        let existing_tar_data = std::fs::read(&initial_path).context("source: read initial tar")?;

        // Build a new tar with existing entries + extra files
        let mut new_tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut new_tar_data);

            // Names the git archive already carries, so an extra file that
            // names one of them is not appended a second time.
            let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Copy all entries from the git archive
            let mut archive = tar::Archive::new(&existing_tar_data[..]);
            for tar_entry in archive.entries().context("source: read tar entries")? {
                let mut tar_entry = tar_entry.context("source: read tar entry")?;
                let header = tar_entry.header().clone();
                if let Ok(name) = tar_entry.path() {
                    written.insert(name.to_string_lossy().replace('\\', "/"));
                }
                let mut data = Vec::new();
                tar_entry
                    .read_to_end(&mut data)
                    .context("source: read tar entry data")?;
                builder
                    .append(&header, &data[..])
                    .context("source: copy tar entry")?;
            }

            // Iterate in archive-path order so two runs against the
            // same input set produce byte-identical tars. The glob
            // expansion that built `extra_files` walks the filesystem
            // in inode order which differs between fresh worktrees.
            let mut sorted_extras: Vec<&SourceFileEntry> = extra_files.iter().collect();
            sorted_extras.sort_by(|a, b| a.src.cmp(&b.src));

            // Add extra files with metadata
            for entry in sorted_extras {
                let src = Path::new(&entry.src);

                // Mirror the zip-branch behavior: missing extras
                // hard-fail under strict mode, warn-and-skip otherwise.
                // Without this guard the `std::fs::File::open` below
                // hard-fails the tar archive regardless of strict mode
                // — a referenced file that moves between releases
                // (CHANGELOG.md renamed, docs reorganized) would kill
                // the source stage instead of degrading gracefully.
                if !src.exists() {
                    if strict {
                        bail!("source: extra file '{}' not found (strict mode)", entry.src);
                    }
                    log.warn(&format!("skipped extra file '{}' — not found", entry.src));
                    continue;
                }

                let dest_rel = extra_dest_rel(entry, repo_root);
                let archive_path = Path::new(prefix).join(&dest_rel);
                // tar accepts duplicate member names silently, so an extra that
                // names an entry `git archive` already wrote would ship twice
                // and unpack in append order.
                if !written.insert(archive_path.to_string_lossy().replace('\\', "/")) {
                    log.warn(&format!(
                        "skipped extra file '{}' — '{}' is already in the archive",
                        entry.src,
                        archive_path.display()
                    ));
                    continue;
                }

                // Read file content
                let mut file_data = Vec::new();
                std::fs::File::open(src)
                    .with_context(|| format!("source: open extra file '{}'", entry.src))?
                    .read_to_end(&mut file_data)
                    .with_context(|| format!("source: read extra file '{}'", entry.src))?;

                // Build tar header from filesystem metadata
                let metadata = std::fs::metadata(src)
                    .with_context(|| format!("source: metadata for '{}'", entry.src))?;
                let mut header = tar::Header::new_gnu();
                header.set_size(file_data.len() as u64);

                // Default mode: normalize to 0o644 when SDE is set
                // (deterministic build mode). Filesystem-derived mode bits
                // are cross-platform divergent: Linux/macOS's
                // `PermissionsExt::mode()` returns the full `st_mode`
                // (S_IFREG | perms — e.g. 0o100644 for a regular file with
                // 0o644 perms), while Windows hardcodes 0o644. tar headers
                // store whatever value is passed verbatim, so the same file
                // produces different tar bytes (and therefore different
                // SHAs) across OS shards — breaking publish-only's
                // cross-shard hash-verify even though every shard produced
                // the "same" archive content. Forcing 0o644 under SDE
                // matches Windows's hardcode and the source-stage
                // default; users needing exec bits inside the source
                // archive should use the `info.mode:` override.
                let default_mode: u32 = if sde_mtime.is_some() {
                    0o644
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode()
                    }
                    #[cfg(not(unix))]
                    {
                        0o644
                    }
                };
                header.set_mode(default_mode);

                // Mtime: SDE if pinned (reproducibility), else
                // filesystem mtime (legacy).
                let default_mtime: u64 = sde_mtime.unwrap_or_else(|| {
                    metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                });
                header.set_mtime(default_mtime);
                if sde_mtime.is_some() {
                    header.set_uid(0);
                    header.set_gid(0);
                    header.set_username("").ok();
                    header.set_groupname("").ok();
                }

                // Apply info overrides if present. A configured owner/group that
                // overflows the 32-byte ustar field must fail loudly rather than
                // be silently dropped to the default.
                if let Some(ref info) = entry.info {
                    if let Some(ref owner) = info.owner {
                        header.set_username(owner).with_context(|| {
                            format!(
                                "source: owner '{owner}' invalid for tar header on '{}'",
                                entry.src
                            )
                        })?;
                    }
                    if let Some(ref group) = info.group {
                        header.set_groupname(group).with_context(|| {
                            format!(
                                "source: group '{group}' invalid for tar header on '{}'",
                                entry.src
                            )
                        })?;
                    }
                    if let Some(mode) = info.mode {
                        header.set_mode(mode.value());
                    }
                    if let Some(ref mtime_str) = info.mtime {
                        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(mtime_str) {
                            header.set_mtime(dt.timestamp() as u64);
                        } else if let Ok(ts) = mtime_str.parse::<u64>() {
                            header.set_mtime(ts);
                        } else if strict {
                            bail!(
                                "source: could not parse mtime '{}' as RFC3339 or unix timestamp (strict mode)",
                                mtime_str
                            );
                        } else {
                            log.warn(&format!(
                                "could not parse mtime '{}' as RFC3339 or unix timestamp",
                                mtime_str
                            ));
                        }
                    }
                }

                header.set_path(&archive_path).with_context(|| {
                    format!("source: set tar path for '{}'", archive_path.display())
                })?;
                header.set_cksum();

                builder
                    .append(&header, &file_data[..])
                    .with_context(|| format!("source: append '{}' to tar", entry.src))?;
            }

            builder.finish().context("source: finish tar")?;
        }

        // Write final output (compressed or plain)
        if git_format == "tar.gz" {
            let gz_file =
                std::fs::File::create(&output_path).context("source: create gzip output file")?;
            let mut encoder =
                flate2::write::GzEncoder::new(gz_file, flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, &new_tar_data)
                .context("source: write gzip data")?;
            encoder.finish().context("source: finish gzip")?;
        } else {
            std::fs::write(&output_path, &new_tar_data).context("source: write tar output")?;
        }
        let _ = std::fs::remove_file(&initial_path);
    }

    Ok(output_path)
}

/// Determine the repository root via `git rev-parse --show-toplevel`.
pub(crate) fn get_repo_root(cwd: &Path, log: &anodizer_core::log::StageLogger) -> Result<PathBuf> {
    log.debug(&format!(
        "running git rev-parse --show-toplevel (cwd: {})",
        cwd.display()
    ));
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("source: failed to run 'git rev-parse --show-toplevel'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("source: failed to determine repo root: {}", stderr.trim());
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(root))
}
