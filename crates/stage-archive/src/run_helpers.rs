//! Archive-stage output plumbing: config validation, archive-mtime
//! resolution, per-format archive writing, per-binary output naming, the
//! `templated_files` staging renderer, and the per-run template-var
//! teardown.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::Artifact;
use anodizer_core::config::{ArchiveConfig, VALID_ARCHIVE_FORMATS};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template_file_render::render_templated_file_entry;

use crate::entries::{ArchiveEntry, write_archive_entries, write_zip_entries};
use crate::formats::{TarEntries, create_gz, create_xz, write_tar_archive, write_zip_archive};
use crate::run::ARCHIVE_TEMPLATED_STAGING_DIR;

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
                        log.warn("format_override has empty os value");
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

/// Render one `(stem, dest, source)` triple per binary for `format: binary`,
/// each named by `name_tmpl` evaluated with THAT binary's `{{ .Binary }}`.
///
/// Leaves `{{ .Binary }}` on the group representative (the first selected
/// binary) so templates evaluated later in the same target iteration see the
/// value they saw before this call.
pub(crate) fn render_binary_outputs<'a>(
    ctx: &mut Context,
    selected_bins: &[&'a Artifact],
    name_tmpl: &str,
    dist: &Path,
    crate_name: &str,
    target: &str,
) -> Result<Vec<(String, PathBuf, &'a Artifact)>> {
    let mut outs = Vec::with_capacity(selected_bins.len());
    for bin in selected_bins {
        ctx.template_vars_mut()
            .set("Binary", &bin.binary_name().unwrap_or_default());
        let stem = ctx.render_template(name_tmpl).with_context(|| {
            format!("archive: render binary name template for {crate_name}/{target}")
        })?;
        if stem.is_empty() {
            bail!(
                "archive: rendered archive name template '{}' produced an \
                 empty stem for binary '{}' of crate '{}' target '{}'. An \
                 empty stem yields the dist directory itself as the output \
                 path, which the duplicate-name detector and downstream \
                 stages cannot resolve. Verify the template references \
                 variables that are populated on this run (e.g. \
                 `{{{{ Tag }}}}` is unset during `--snapshot` — use \
                 `{{{{ Version }}}}` or the default \
                 `archive.name_template` instead).",
                name_tmpl,
                bin.binary_name().unwrap_or_default(),
                crate_name,
                target
            );
        }
        let stem = anodizer_core::archive_name::binary_output_name(stem, target);
        let dest = dist.join(&stem);
        outs.push((stem, dest, *bin));
    }
    // Restore the group-representative `.Binary` the rest of
    // this iteration's templates expect.
    if let Some(bin) = selected_bins.first() {
        ctx.template_vars_mut()
            .set("Binary", &bin.binary_name().unwrap_or_default());
    }
    Ok(outs)
}

/// The resolved `(src, archive_name, info)` entries an archive entry writes,
/// after `files:` globs and `templated_files` staging have been applied.
struct ResolvedEntries {
    entries: Vec<ArchiveEntry>,
    mtime: Option<u64>,
    strict: bool,
}

impl TarEntries for ResolvedEntries {
    fn write_into<W: std::io::Write>(self, tar: &mut tar::Builder<W>, label: &str) -> Result<()> {
        write_archive_entries(tar, &self.entries, self.mtime, label, self.strict)
    }
}

pub(crate) fn write_archive_in_format(
    format: &str,
    archive_path: &Path,
    all_entries: &[&ArchiveEntry],
    path_refs: &[&Path],
    source_date_epoch: Option<u64>,
    strict: bool,
    log: &StageLogger,
) -> Result<()> {
    match format {
        "zip" => {
            write_zip_archive(archive_path, |zip| {
                write_zip_entries(
                    zip,
                    &entries_to_owned(all_entries),
                    source_date_epoch,
                    strict,
                )
            })?;
        }
        "tar.gz" | "tgz" | "tar.xz" | "txz" | "tar.zst" | "tzst" | "tar" => {
            let tar_format = match format {
                "tgz" => "tar.gz",
                "txz" => "tar.xz",
                "tzst" => "tar.zst",
                other => other,
            };
            write_tar_archive(
                tar_format,
                archive_path,
                ResolvedEntries {
                    entries: entries_to_owned(all_entries),
                    mtime: source_date_epoch,
                    strict,
                },
            )?;
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
        other => bail!("unsupported archive format: {other}"),
    }
    Ok(())
}

pub(crate) fn resolve_archive_mtime(ctx: &anodizer_core::context::Context) -> Option<u64> {
    ctx.resolve_reproducible_mtime()
}

pub(crate) fn clear_archive_template_vars(ctx: &mut anodizer_core::context::Context) {
    let tvars = ctx.template_vars_mut();
    tvars.set("Os", "");
    tvars.set("Arch", "");
    tvars.set("Target", "");
    tvars.set("Binary", "");
    // `Shell` is bound transiently during mode-A completion generation; clear
    // it so it does not leak into downstream stages' template scope.
    tvars.set("Shell", "");
    tvars.set("ArtifactName", "");
    tvars.set("ArtifactPath", "");
    tvars.set("ArtifactExt", "");
    tvars.set("ArtifactID", "");
}

/// Render every `archives[].templated_files[]` entry into a staging
/// directory and return one [`ArchiveEntry`] per rendered file so the
/// archive packer treats them as ordinary contents.
///
/// Per-entry `skip:` is consulted up front; the source path, content
/// body, and destination path are all template-rendered so each archive
/// can shape its dst based on `.Os`, `.Arch`, `.Format`, etc. Non-UTF8
/// source files emit a clear error instead of the cryptic
/// "stream did not contain valid UTF-8" surfaced by `read_to_string`.
pub(crate) fn render_archive_templated_files(
    ctx: &mut Context,
    entries: &[anodizer_core::config::TemplateFileConfig],
    archive_id: &str,
    target: &str,
    format: &str,
    dist: &Path,
) -> Result<Vec<ArchiveEntry>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    // One staging dir per (archive_id, target, format) so multiple
    // formats for the same archive write to distinct paths.
    let staging = dist
        .join(ARCHIVE_TEMPLATED_STAGING_DIR)
        .join(archive_id)
        .join(target)
        .join(format);
    fs::create_dir_all(&staging).with_context(|| {
        format!(
            "archive: create templated_files staging dir '{}'",
            staging.display()
        )
    })?;

    let mut out: Vec<ArchiveEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = entry.id.as_deref().unwrap_or("default");
        let label = format!("archives[{archive_id}].templated_files[{id}]");

        let render = match render_templated_file_entry(ctx, entry, &label)? {
            Some(r) => r,
            None => continue,
        };

        let out_path = staging.join(&render.rendered_dst);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("{label}: create parent dir '{}'", parent.display()))?;
        }
        fs::write(&out_path, &render.rendered_contents)
            .with_context(|| format!("{label}: write '{}'", out_path.display()))?;

        out.push(ArchiveEntry {
            src: out_path,
            archive_name: PathBuf::from(&render.rendered_dst),
            info: None,
        });
    }
    Ok(out)
}
