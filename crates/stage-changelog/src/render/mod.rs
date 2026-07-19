//! Markdown rendering and the `render_crate_section` public entry point.
//!
//! `render_changelog_with_provider` is the canonical render function
//! (used by both the in-pipeline `Stage::run` body and the
//! `bump --commit` flow in `render_crate_section`). The recursive
//! `render_groups` walks the `GroupedCommits` tree to produce headings +
//! bullets at the configured Markdown depth.
//!
//! `merge_into_changelog` is the file-level merge helper used by
//! `render_crate_section` to fold a new release into an existing
//! `CHANGELOG.md`. It detects the [Keep a Changelog] shape (a
//! `## [Unreleased]` heading): in that mode it promotes the
//! `## [Unreleased]` section to the released version, inserts a fresh
//! empty `## [Unreleased]`, and rolls the `[Unreleased]` / `[<version>]`
//! compare-link footer. Otherwise it falls back to splicing a new
//! `## [<version>]` section directly after the leading H1.
//!
//! [Keep a Changelog]: https://keepachangelog.com/

use std::sync::LazyLock;

use anodizer_core::config::ChangelogGroup;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use regex::Regex;
use serde_json::Value as JsonValue;

use crate::fetch::relative_filter;
use crate::group::{
    CommitInfo, GroupedCommits, apply_filters, apply_include_filters, compile_group_regexes,
    exclude_filters_with_version_sync, extract_co_authors, group_commits, parse_commit_message,
    sort_commits,
};

mod commit;
mod crate_changelog;
mod entry;
mod merge;
mod promote;
mod refresh;
mod section;

pub(crate) use commit::*;
pub(crate) use crate_changelog::*;
pub use entry::*;
pub(crate) use merge::*;
pub(crate) use promote::*;
pub use refresh::*;
pub use section::*;

#[cfg(test)]
mod render_extra_tests;
#[cfg(test)]
mod root_section_tests;
