//! Homebrew formula + cask publisher.
//!
//! Module layout:
//! - [`formula`] — Tera template + `FormulaOptions` + `generate_formula*`.
//! - [`cask`] — cask Tera template + `CaskParams` + `generate_cask*`.
//! - [`commit_msg`] — shared commit-message renderer (used by aur, scoop,
//!   krew, nix, aur_source publishers as well).
//! - [`publish_formula`] — `publish_to_homebrew` (per-crate formula + optional
//!   same-tap cask).
//! - [`publish_cask`] — `publish_cask` (standalone per-crate cask).
//! - [`publish_top`] — `publish_top_level_homebrew_casks` (top-level
//!   `homebrew_casks:` config).

mod cask;
mod commit_msg;
mod formula;
mod publish_cask;
mod publish_formula;
mod publish_top;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use cask::{
    CaskArchEntry, CaskParams, CaskPlatformBlock, generate_cask, render_generate_completions,
};
pub(crate) use cask::{CaskGenResult, crate_has_macos_cask_artifact};
pub(crate) use commit_msg::{render_commit_msg, render_commit_msg_with_prev};
pub use formula::{
    FormulaOptions, build_completion_and_manpage_install_lines, generate_formula,
    generate_formula_with_opts, render_formula_license, render_livecheck,
};
pub use publish_cask::publish_cask;
pub(crate) use publish_cask::render_homebrew_cask_for_crate;
pub use publish_formula::publish_to_homebrew;
pub(crate) use publish_formula::{
    crate_has_homebrew_archives, render_homebrew_formula_for_crate, render_same_tap_cask_for_crate,
};
pub use publish_top::publish_top_level_homebrew_casks;
pub(crate) use publish_top::render_top_level_homebrew_casks;
pub(crate) use publisher::is_homebrew_per_crate_configured;

use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

/// Resolve the cask `directory:` field to its rendered, on-tap subdirectory.
///
/// Defaults to `"Casks"` (the homebrew-cask auto-discovery convention) when
/// unset, then renders the value through the template engine. A Tera render
/// failure PROPAGATES rather than being swallowed: a swallowed error would
/// leave the literal `{{ … }}` template as a directory name and commit + push
/// it to the tap, producing an unusable cask path.
pub(crate) fn resolve_cask_directory(directory: Option<&str>, ctx: &Context) -> Result<String> {
    let directory_raw = directory.unwrap_or("Casks");
    ctx.render_template(directory_raw).with_context(|| {
        format!(
            "homebrew cask: render `directory` template '{}'",
            directory_raw
        )
    })
}
