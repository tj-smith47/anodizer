//! Pre-flight publisher-state types shared between `core` and `stage-publish`.
//!
//! The preflight check runs before any stage in the release pipeline to detect
//! one-way-door publishers (crates.io, Chocolatey, WinGet, AUR) that already
//! have the target version submitted or approved. Discovering this before the
//! build prevents an entire wasted release cycle.
//!
//! # State machine
//!
//! ```text
//! Clean      → safe to publish
//! Published  → idempotent skip (not a blocker)
//! InModeration { reason } → blocker (version submitted, moderation queue)
//! PRPending  → blocker (PR already open for this version)
//! Unknown { reason } → warn-and-allow unless --strict-preflight
//! ```

use std::fmt;

// ---------------------------------------------------------------------------
// PublisherState
// ---------------------------------------------------------------------------

/// The state of a single publisher for the target version.
#[derive(Debug, Clone, PartialEq)]
pub enum PublisherState {
    /// Version not present. Safe to publish.
    Clean,
    /// Version already published / approved. Idempotent skip (not a blocker).
    Published,
    /// Submitted but pending review / moderation. Blocker. `reason` is a
    /// short human-readable explanation (e.g. "package in moderation queue").
    InModeration { reason: String },
    /// PR already open against the upstream registry. Blocker.
    PRPending(String),
    /// Couldn't determine state. Warn-and-allow unless `--strict-preflight`.
    /// `reason` carries a short error description for diagnostics.
    Unknown { reason: String },
}

impl PublisherState {
    /// Returns `true` when this state blocks the release.
    ///
    /// `InModeration` and `PRPending` are hard blockers.
    /// `Unknown` only blocks when `strict` is `true`.
    pub fn is_blocker(&self, strict: bool) -> bool {
        match self {
            PublisherState::InModeration { .. } | PublisherState::PRPending(_) => true,
            PublisherState::Unknown { .. } => strict,
            _ => false,
        }
    }

    /// A short human-readable label for table output.
    pub fn label(&self) -> &'static str {
        match self {
            PublisherState::Clean => "clean",
            PublisherState::Published => "published",
            PublisherState::InModeration { .. } => "in-moderation",
            PublisherState::PRPending(_) => "pr-pending",
            PublisherState::Unknown { .. } => "unknown",
        }
    }
}

impl fmt::Display for PublisherState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublisherState::Clean => write!(f, "clean"),
            PublisherState::Published => write!(f, "already published (idempotent skip)"),
            PublisherState::InModeration { reason } => {
                write!(f, "in moderation queue: {} — BLOCKER", reason)
            }
            PublisherState::PRPending(url) => write!(f, "PR already open: {} — BLOCKER", url),
            PublisherState::Unknown { reason } => write!(f, "unknown ({})", reason),
        }
    }
}

// ---------------------------------------------------------------------------
// PreflightEntry
// ---------------------------------------------------------------------------

/// One publisher's result in the preflight report.
#[derive(Debug, Clone)]
pub struct PreflightEntry {
    /// Short publisher name for display (e.g. "cargo", "chocolatey").
    pub publisher: String,
    /// Crate / package name being checked.
    pub package: String,
    /// Version that was queried.
    pub version: String,
    /// Result of the state query.
    pub state: PublisherState,
}

// ---------------------------------------------------------------------------
// PreflightReport
// ---------------------------------------------------------------------------

/// Aggregated results for all one-way-door publishers.
///
/// `entries` carries one row per checked publisher (cargo / chocolatey /
/// winget / aur). `warnings` and `blockers` are free-form, publisher-agnostic
/// messages produced by the release-resilience preflight extension: rollback
/// token scope checks (Task 18) and per-publisher `Publisher::preflight()`
/// hook results. The two channels are kept separate from `entries` so that
/// the existing one-way-door consumers (state-machine queries like
/// `has_blockers` / `clean_count`) stay focused on publisher state, while the
/// CLI's operator-facing output can still surface every warning and blocker
/// the preflight pipeline produced.
#[derive(Debug, Default)]
pub struct PreflightReport {
    pub entries: Vec<PreflightEntry>,
    /// Non-blocking concerns surfaced during preflight (missing rollback
    /// scope in default mode, `Publisher::preflight()` returning Warning).
    pub warnings: Vec<String>,
    /// Hard blockers surfaced during preflight (missing rollback scope in
    /// `--strict` mode, `Publisher::preflight()` returning Blocker).
    pub blockers: Vec<String>,
}

impl PreflightReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: PreflightEntry) {
        self.entries.push(entry);
    }

    /// Entries whose state is `Clean`.
    pub fn clean_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.state == PublisherState::Clean)
            .count()
    }

    /// Whether any entry is a blocker given the strict flag.
    pub fn has_blockers(&self, strict: bool) -> bool {
        self.entries.iter().any(|e| e.state.is_blocker(strict))
    }

    /// Entries that are blockers.
    pub fn blockers(&self, strict: bool) -> Vec<&PreflightEntry> {
        self.entries
            .iter()
            .filter(|e| e.state.is_blocker(strict))
            .collect()
    }
}

impl fmt::Display for PreflightReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Pre-flight publisher check:")?;
        for entry in &self.entries {
            writeln!(
                f,
                "  [{:>14}]  {} {}@{}",
                entry.state.label(),
                entry.publisher,
                entry.package,
                entry.version
            )?;
            // Print extra detail for states that carry context.
            match &entry.state {
                PublisherState::PRPending(url) => {
                    writeln!(f, "               PR: {}", url)?;
                }
                PublisherState::Unknown { reason } | PublisherState::InModeration { reason } => {
                    writeln!(f, "               reason: {}", reason)?;
                }
                _ => {}
            }
        }
        // Surface free-form warnings/blockers from the resilience extension
        // (rollback-scope checks + `Publisher::preflight()` results) so they
        // flow through the same Display channel the CLI prints. Suppressed
        // when both are empty to preserve the existing one-line-per-entry
        // cadence for clean reports.
        for w in &self.warnings {
            writeln!(f, "  [       warning]  {}", w)?;
        }
        for b in &self.blockers {
            writeln!(f, "  [       blocker]  {}", b)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(publisher: &str, state: PublisherState) -> PreflightEntry {
        PreflightEntry {
            publisher: publisher.to_string(),
            package: "mypkg".to_string(),
            version: "1.2.3".to_string(),
            state,
        }
    }

    #[test]
    fn report_aggregation_four_publishers() {
        // Mock 4 publishers, one in each non-trivial state, assert categorisation.
        let mut report = PreflightReport::new();
        report.push(entry("cargo", PublisherState::Clean));
        report.push(entry(
            "chocolatey",
            PublisherState::InModeration {
                reason: "package in moderation queue".into(),
            },
        ));
        report.push(entry(
            "winget",
            PublisherState::PRPending("https://github.com/microsoft/winget-pkgs/pull/123".into()),
        ));
        report.push(entry(
            "aur",
            PublisherState::Unknown {
                reason: "AUR RPC returned 503".into(),
            },
        ));

        // clean_count
        assert_eq!(report.clean_count(), 1);

        // non-strict: Unknown is not a blocker
        assert!(report.has_blockers(false));
        let blockers = report.blockers(false);
        assert_eq!(blockers.len(), 2);
        assert!(blockers.iter().any(|e| e.publisher == "chocolatey"));
        assert!(blockers.iter().any(|e| e.publisher == "winget"));

        // strict: Unknown also blocks
        assert!(report.has_blockers(true));
        let strict_blockers = report.blockers(true);
        assert_eq!(strict_blockers.len(), 3);
    }

    #[test]
    fn report_all_clean_no_blockers() {
        let mut report = PreflightReport::new();
        report.push(entry("cargo", PublisherState::Clean));
        report.push(entry("aur", PublisherState::Clean));

        assert!(!report.has_blockers(false));
        assert!(!report.has_blockers(true));
        assert_eq!(report.clean_count(), 2);
    }

    #[test]
    fn published_is_not_blocker() {
        let mut report = PreflightReport::new();
        report.push(entry("cargo", PublisherState::Published));

        assert!(!report.has_blockers(false));
        assert!(!report.has_blockers(true));
    }

    #[test]
    fn unknown_only_blocks_when_strict() {
        let mut report = PreflightReport::new();
        report.push(entry(
            "aur",
            PublisherState::Unknown {
                reason: "timeout".into(),
            },
        ));

        assert!(!report.has_blockers(false));
        assert!(report.has_blockers(true));
    }

    #[test]
    fn display_includes_blocker_label() {
        let mut report = PreflightReport::new();
        report.push(entry(
            "chocolatey",
            PublisherState::InModeration {
                reason: "package in moderation queue".into(),
            },
        ));

        let s = report.to_string();
        assert!(s.contains("in-moderation"), "display: {s}");
        assert!(s.contains("chocolatey"), "display: {s}");
    }
}
