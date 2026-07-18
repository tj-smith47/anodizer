//! Pre-flight publisher-state queries for one-way-door publishers.
//!
//! Runs before the release pipeline to detect versions already submitted /
//! approved / in moderation, preventing a wasted release cycle.
//!
//! ## Checked publishers
//!
//! | Publisher    | One-way door? | Check mechanism                             |
//! |--------------|---------------|---------------------------------------------|
//! | crates.io    | yes           | Sparse index HTTPS GET                      |
//! | Chocolatey   | yes           | NuGet V2 OData feed                         |
//! | WinGet       | yes           | GitHub API — open PRs + fork branch          |
//! | AUR          | informational | AUR RPC v5 info endpoint                    |
//!
//! Cloudsmith is intentionally excluded: versions can be re-uploaded.

mod checkers;
mod simulation;

pub use checkers::*;
use simulation::*;

#[cfg(test)]
use anodizer_core::retry::RetryPolicy;

#[cfg(test)]
mod tests;
