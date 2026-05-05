//! Changelog generation stage.
//!
//! Produces per-crate Markdown changelogs from git history (or SCM compare APIs)
//! and writes a combined `dist/CHANGELOG.md`. Also exposes a `render_crate_section`
//! entry point used by `anodizer bump --commit` to bundle a changelog edit
//! alongside the version bump in a single commit.

mod fetch;
mod group;
mod render;
mod run;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Public API re-exports
// ---------------------------------------------------------------------------

pub use render::{ChangelogUpdate, InsertionMode, render_crate_section};

// ---------------------------------------------------------------------------
// ChangelogStage — pipeline entry point
// ---------------------------------------------------------------------------

pub struct ChangelogStage;
