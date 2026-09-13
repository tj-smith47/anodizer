//! Archive stage — bundles per-crate binaries (and extra files) into
//! tar/zip/gz archives.
//!
//! Public surface:
//! - [`ArchiveStage`] — the [`Stage`](anodizer_core::stage::Stage) driver.
//! - File-spec resolution: [`ResolvedExtraFile`], [`resolve_file_specs`].
//! - Format primitives: [`copy_binary`], [`create_gz`], [`create_tar`],
//!   [`create_tar_gz`], [`create_tar_xz`], [`create_tar_zst`], [`create_zip`],
//!   [`resolve_glob_patterns`].

use anodizer_core::log::{StageLogger, Verbosity};

mod archive_config;
mod completions_gen;
mod entries;
mod file_specs;
mod formats;
mod plan;
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
// default_name_template
// ---------------------------------------------------------------------------

pub(crate) fn default_name_template() -> &'static str {
    anodizer_core::archive_name::DEFAULT_NAME_TEMPLATE
}

/// Multi-crate variant of [`default_name_template`]: identical to the
/// canonical template, but relies on the archive stage to override the
/// `ProjectName` template var to the per-crate name so each crate's archive
/// stem is distinct without forcing every user to hand-author
/// `archive.name_template:`. `{{ .CrateName }}` remains separately available
/// for templates that need to disambiguate further. Single-crate configs use
/// [`default_name_template`] (same shape) with the workspace `ProjectName`
/// untouched.
pub(crate) fn default_name_template_multi_crate() -> &'static str {
    anodizer_core::archive_name::DEFAULT_NAME_TEMPLATE_MULTI_CRATE
}

pub(crate) fn default_binary_name_template() -> &'static str {
    anodizer_core::archive_name::DEFAULT_BINARY_NAME_TEMPLATE
}

// ---------------------------------------------------------------------------
// ArchiveStage
// ---------------------------------------------------------------------------

pub struct ArchiveStage;
