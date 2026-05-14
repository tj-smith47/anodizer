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
pub(crate) use commit_msg::{render_commit_msg, render_commit_msg_with_prev};
pub use formula::{FormulaOptions, generate_formula, generate_formula_with_opts};
pub use publish_cask::publish_cask;
pub use publish_formula::publish_to_homebrew;
pub use publish_top::publish_top_level_homebrew_casks;
