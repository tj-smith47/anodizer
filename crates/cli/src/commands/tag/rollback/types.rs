/// A published-state guard refusal: the rollback was declined BY DESIGN
/// because destroying the tag(s) could only orphan live published state
/// (a one-way-door registry already holds the version). Distinct from a
/// mechanical rollback failure (git error, unreachable network probe,
/// unmappable config): a refusal is final protection with a known next
/// step, not breakage. Callers that drive rollback programmatically
/// (the release failure policy) downcast to this type to render the
/// refusal as protective status output instead of a failure warning.
#[derive(Debug)]
pub struct RollbackRefusal {
    /// Why the rollback was refused — the burn evidence, one line per
    /// affected tag/version.
    pub reason: String,
    /// What the operator should do instead (fix forward / `--force`).
    pub next_step: String,
}

impl std::fmt::Display for RollbackRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to roll back — {}\nnext step: {}",
            self.reason, self.next_step
        )
    }
}

impl std::error::Error for RollbackRefusal {}

/// Canonical fix-forward guidance shared by every refusal site: the
/// version is burned, so the only clean path is the NEXT version;
/// `--force` remains the explicit override.
pub(super) fn refusal_next_step() -> String {
    "fix the failure and cut the NEXT version (auto-tag mints it from the next push). \
     To override anyway: `anodizer tag rollback --force`."
        .to_string()
}

/// Scope filter for which tag shape(s) to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Both lockstep (`vX.Y.Z`) and per-crate (`<crate>-vX.Y.Z`) tags.
    All,
    /// Only lockstep tags (`vX.Y.Z`).
    Lockstep,
    /// Only per-crate tags (`<crate>-vX.Y.Z`).
    PerCrate,
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "all" => Ok(Scope::All),
            "lockstep" => Ok(Scope::Lockstep),
            "per-crate" | "percrate" => Ok(Scope::PerCrate),
            other => Err(format!(
                "invalid --scope value: {other:?} (expected all | lockstep | per-crate)"
            )),
        }
    }
}

/// Rollback strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `git revert --no-edit <sha>` — preserves history. Default.
    Revert,
    /// `git reset --hard <sha>~1` — rewrites history; requires
    /// `--force-with-lease` to push. Opt-in only.
    Reset,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "revert" => Ok(Mode::Revert),
            "reset" => Ok(Mode::Reset),
            other => Err(format!(
                "invalid --mode value: {other:?} (expected revert | reset)"
            )),
        }
    }
}

pub struct RollbackOpts {
    /// Target SHA. `None` resolves to `HEAD`.
    pub sha: Option<String>,
    pub dry_run: bool,
    pub no_push: bool,
    /// `--force`: override the published-state guard. Without it,
    /// rollback refuses when the tag's run summary shows a one-way-door
    /// (Submitter) publisher landed — the version is burned at a
    /// registry that never accepts the same version twice — when the
    /// crates.io index shows the tag's crate@version live (GLOBAL state:
    /// a prior run may have published it; an unreachable index fails
    /// closed) — or, when no summary exists, when the tag's GitHub
    /// release is published (non-draft).
    pub force: bool,
    pub scope: Scope,
    pub mode: Mode,
    /// Branch to push the revert commit to. `None` triggers
    /// auto-resolution via [`git::get_current_branch_in`]; a hard
    /// failure surfaces when HEAD is detached and no local branch
    /// points at it (the operator must pass `--branch` explicitly).
    pub branch: Option<String>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}
