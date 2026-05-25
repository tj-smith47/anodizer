//! Archive stage — bundles per-crate binaries (and extra files) into
//! tar/zip/gz archives.
//!
//! Public surface:
//! - [`ArchiveStage`] — the [`Stage`] driver.
//! - [`formats_for_target`] / [`format_for_target`] — apply OS-based format overrides.
//! - File-spec resolution: [`ResolvedExtraFile`], [`resolve_file_specs`].
//! - Format primitives: [`copy_binary`], [`create_gz`], [`create_tar`],
//!   [`create_tar_gz`], [`create_tar_xz`], [`create_tar_zst`], [`create_zip`],
//!   [`resolve_glob_patterns`].

use anodizer_core::config::FormatOverride;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::target::map_target;

mod entries;
mod file_specs;
mod formats;
mod run;
mod run_helpers;

#[cfg(test)]
mod tests;

pub use file_specs::{ResolvedExtraFile, resolve_file_specs};
pub use formats::{
    copy_binary, create_gz, create_tar, create_tar_gz, create_tar_xz, create_tar_zst, create_zip,
    resolve_glob_patterns,
};

// File-spec resolution (longest_common_prefix, render_file_info,
// ResolvedExtraFile, resolve_file_specs, resolve_default_extra_files) lives
// in `file_specs.rs`. Naming utilities (normalize_archive_path,
// compute_archive_name) live in `formats.rs`.
//
// ArchiveEntry, deduplicate_entries, sort_entries, write_archive_entries,
// write_zip_entries live in `entries.rs`.

/// Module-level logger for warnings emitted from helpers that don't have
/// runtime access to the stage's `ctx.logger("archive")`. Uses `Verbosity::Normal`
/// so warnings are always shown except in `--quiet` mode (which the helpers
/// can't observe). Routes through StageLogger for consistent `[archive]` framing.
pub(crate) fn archive_log() -> StageLogger {
    StageLogger::new("archive", Verbosity::Normal)
}

// ---------------------------------------------------------------------------
// format_for_target
// ---------------------------------------------------------------------------

/// Determine the archive format(s) for a target, applying OS-based overrides.
/// Returns the override's `formats` list when an override matches the target's OS,
/// otherwise falls back to `default_format`.
pub fn formats_for_target(
    target: &str,
    default_format: &str,
    overrides: &[FormatOverride],
) -> Vec<String> {
    let (os, _arch) = map_target(target);
    for ov in overrides {
        // GR-aligned (archive.go:349 `strings.HasPrefix(platform, override.Goos)`):
        // FormatOverride.os matches when the resolved target's os field starts
        // with the configured value. Same call-site rationale as the primary
        // archive run loop.
        //
        // Empty `os` is rejected as a user typo (anodizer-stricter than GR,
        // which lets `os: ""` match every target via empty-prefix). A user who
        // accidentally writes `os:` (yaml-empty) gets a clean fallback to the
        // default format instead of a silent global-override.
        if !ov.os.is_empty()
            && os.starts_with(&ov.os)
            && let Some(ref fmts) = ov.formats
            && !fmts.is_empty()
        {
            return fmts.clone();
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

pub(crate) fn default_name_template() -> &'static str {
    "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

/// Multi-crate variant of [`default_name_template`]: identical to the
/// canonical GR template, but relies on the archive stage to override the
/// `ProjectName` template var to the per-crate name so each crate's archive
/// stem is distinct without forcing every user to hand-author
/// `archive.name_template:`. `{{ .CrateName }}` remains separately available
/// for templates that need to disambiguate further. Single-crate configs use
/// [`default_name_template`] (same shape) with the workspace `ProjectName`
/// untouched.
pub(crate) fn default_name_template_multi_crate() -> &'static str {
    "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

pub(crate) fn default_binary_name_template() -> &'static str {
    "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

// ---------------------------------------------------------------------------
// ArchiveStage
// ---------------------------------------------------------------------------

pub struct ArchiveStage;
