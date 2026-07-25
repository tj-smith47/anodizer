//! `anodize tag rollback` — delete anodize-managed tags at a SHA and
//! revert (or reset to) the bump commit they point at.
//!
//! Failure-recovery counterpart to `anodize tag`: when a downstream
//! `anodize release` poisons a tag (publish failure, mcp 422, etc.) the
//! operator is left with a tag pointing at a bumped-but-broken commit.
//! This subcommand deletes the tag locally + on origin, then either
//! `git revert`s the bump commit (default, history-preserving) or
//! `git reset --hard`s past it (opt-in, history-rewriting).
//!
//! Between the published-state guard and the destructive git work sits
//! the publisher unwind ([`unwind`]): every publisher the prior release
//! run recorded gets its `rollback()` re-invoked, so a withdrawal closes
//! the tap PRs and deletes the mirrored artifacts instead of leaving them
//! pointing at a tag that no longer exists.
//!
//! Safety rails:
//! - Tag name regex filter — only anodize-shaped tags are touched
//!   (`vX.Y.Z[-pre][+build]` for lockstep, `<crate>-vX.Y.Z[...]` for
//!   per-crate). Non-matching tags are skipped with a reason printed.
//! - Hard-fail when non-anodize commits sit between the target SHA and
//!   HEAD in `--mode=revert` (protects against rolling back a bump
//!   after unrelated work landed on top). Use `--mode=reset` to force.

mod deletion;
mod guard;
mod registry_probe;
mod release_probe;
mod run;
mod tags;
mod types;
mod unwind;

pub use run::run;
pub use types::{Mode, RollbackOpts, Scope};

#[cfg(test)]
mod tests;

/// Trim a SHA to the canonical 7-char short form for log output.
pub(super) fn short(sha: &str) -> &str {
    if sha.len() > 7 { &sha[..7] } else { sha }
}

/// First line of a multi-line commit message, for compact status lines.
pub(super) fn first_line(msg: &str) -> &str {
    msg.lines().next().unwrap_or(msg)
}
