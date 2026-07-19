//! homebrew-core formula-bump publisher.
//!
//! Bumps an EXISTING formula in `Homebrew/homebrew-core` (or any formula
//! repository override) purely through the GitHub API — no clone (the core
//! repo is multi-gigabyte), no `brew` invocation. Mirrors the semantics of
//! `mislav/bump-homebrew-formula-action`: rewrite the formula's `url` /
//! `sha256` / `version` (or `tag:` + `revision:` for git-based formulae),
//! commit through the contents API, and open a fork-based pull request
//! (direct commit only for personal formula repos that opt in).
//!
//! Submodules:
//! - [`formula`] — pure formula-text rewrite + path layout helpers.
//! - [`api`] — the minimal GitHub REST surface (contents / refs / forks /
//!   pulls) the bump drives.
//! - [`locate`] — formula-file location (explicit `path:` override or
//!   sharded/flat layout fallback).
//! - [`publisher`] — orchestration + the `Publisher` trait impl.

pub(crate) mod api;
pub(crate) mod formula;
mod locate;
pub mod publisher;

#[cfg(test)]
mod tests;
