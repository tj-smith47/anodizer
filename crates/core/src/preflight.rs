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
//! InModeration { reason } → reported, never blocking (version submitted, moderation queue)
//! PRPending  → reported, never blocking (PR already open for this version)
//! Unknown { reason } → warn-and-allow unless --strict
//! ```

use std::fmt;

use crate::log::StageLogger;

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
    /// Submitted but pending review / moderation. Reported, never blocking:
    /// the publisher's own `reconcile()` decides whether to skip or dispatch.
    /// `reason` is a short human-readable explanation.
    InModeration { reason: String },
    /// PR already open against the upstream registry. Reported, never
    /// blocking — an open PR is exactly what a converged re-run expects.
    PRPending(String),
    /// Couldn't determine state. `reason` carries a short error description
    /// for diagnostics.
    Unknown { reason: String },
}

impl PublisherState {
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

    /// Which `StageLogger` register a report row for this state renders
    /// under. `InModeration` and `PRPending` are reported states, not
    /// failures — the publisher's own `reconcile()` decides whether to skip
    /// or dispatch, so only `Published` earns the `✓` success marker.
    pub fn row_kind(&self) -> RowKind {
        match self {
            PublisherState::Published => RowKind::Ok,
            PublisherState::Clean
            | PublisherState::InModeration { .. }
            | PublisherState::PRPending(_)
            | PublisherState::Unknown { .. } => RowKind::Info,
        }
    }

    /// Trailing summary text for a report row, e.g. `"in-moderation —
    /// package in moderation queue"`.
    pub fn row_summary(&self) -> String {
        match self {
            PublisherState::Clean => "clean".to_string(),
            PublisherState::Published => "published".to_string(),
            PublisherState::InModeration { reason } => format!("in-moderation — {reason}"),
            PublisherState::PRPending(url) => format!("pr-pending — {url}"),
            PublisherState::Unknown { reason } => format!("unknown — {reason}"),
        }
    }
}

/// Which `StageLogger` register a preflight report row renders under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Green `✓` — `log.success`.
    Ok,
    /// Cyan `•` — `log.status`.
    Info,
}

impl fmt::Display for PublisherState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublisherState::Clean => write!(f, "clean"),
            PublisherState::Published => write!(f, "already published (idempotent skip)"),
            PublisherState::InModeration { reason } => {
                write!(f, "in moderation queue: {}", reason)
            }
            PublisherState::PRPending(url) => write!(f, "PR already open: {}", url),
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
/// token scope checks and per-publisher `Publisher::preflight()` hook
/// results. The two channels are kept separate from `entries` so that
/// the report-only publisher-state channel (queried via `clean_count`)
/// stays focused on publisher state, while the
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

    /// One `(kind, text)` row per entry, ready for `StageLogger` dispatch.
    ///
    /// Each `text` is the subject `{publisher} {package}@{version}`
    /// left-aligned and padded to the width of the widest subject present,
    /// followed by two spaces and [`PublisherState::row_summary`], so the
    /// summaries line up in a column across every row.
    pub fn entry_rows(&self) -> Vec<(RowKind, String)> {
        let subjects: Vec<String> = self
            .entries
            .iter()
            .map(|e| format!("{} {}@{}", e.publisher, e.package, e.version))
            .collect();
        let width = subjects.iter().map(String::len).max().unwrap_or(0);
        self.entries
            .iter()
            .zip(subjects)
            .map(|(entry, subject)| {
                (
                    entry.state.row_kind(),
                    format!("{subject:width$}  {}", entry.state.row_summary()),
                )
            })
            .collect()
    }

    /// Render this report through `log`, one `StageLogger` call per row.
    ///
    /// Publisher entries route through [`PublisherState::row_kind`]
    /// (`✓`/`•`); free-form `warnings` and `blockers` from the
    /// release-resilience preflight extension route through the logger's
    /// own `Warning` / `Error` labels.
    pub fn emit(&self, log: &StageLogger) {
        log.status("Pre-flight publisher check");
        for (kind, text) in self.entry_rows() {
            match kind {
                RowKind::Ok => log.success(&text),
                RowKind::Info => log.status(&text),
            }
        }
        for w in &self.warnings {
            log.warn(w);
        }
        for b in &self.blockers {
            log.error(b);
        }
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

    /// Preflight is report-only: every publisher state, including the two
    /// that used to hard-abort a release, must survive into the report the
    /// operator reads and none of them may gate. Convergence moved the
    /// decision to each publisher's own `reconcile()`, and a pending PR or
    /// moderation entry is exactly what a re-run of an in-flight release is
    /// supposed to find.
    #[test]
    fn report_aggregation_four_publishers() {
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

        assert_eq!(report.clean_count(), 1);
        assert_eq!(report.entries.len(), 4);
        let rows = report.entry_rows();
        for label in ["clean", "in-moderation", "pr-pending", "unknown"] {
            assert!(
                rows.iter().any(|(_, text)| text.contains(label)),
                "every state must reach the operator's report: {label} missing from {rows:?}"
            );
        }
    }

    /// State→`RowKind` mapping: only `Published` earns the `Ok` (`✓`)
    /// marker. `InModeration` and `PRPending` are reported states, not
    /// blockers — a converged re-run expects to find them.
    #[test]
    fn row_kind_matches_state() {
        assert_eq!(PublisherState::Published.row_kind(), RowKind::Ok);
        assert_eq!(PublisherState::Clean.row_kind(), RowKind::Info);
        assert_eq!(
            PublisherState::InModeration {
                reason: "queue".into()
            }
            .row_kind(),
            RowKind::Info
        );
        assert_eq!(
            PublisherState::PRPending("https://example.com/pr/1".into()).row_kind(),
            RowKind::Info
        );
        assert_eq!(
            PublisherState::Unknown {
                reason: "503".into()
            }
            .row_kind(),
            RowKind::Info
        );
    }

    /// Subjects of different lengths must still align: every row's summary
    /// starts at the same column.
    #[test]
    fn entry_rows_align_summaries_to_widest_subject() {
        let mut report = PreflightReport::new();
        report.push(PreflightEntry {
            publisher: "cargo".to_string(),
            package: "cfgd".to_string(),
            version: "0.6.0".to_string(),
            state: PublisherState::Clean,
        });
        report.push(PreflightEntry {
            publisher: "chocolatey".to_string(),
            package: "cfgd-core".to_string(),
            version: "0.6.0".to_string(),
            state: PublisherState::Published,
        });

        let rows = report.entry_rows();
        assert_eq!(rows.len(), 2);
        let starts: Vec<usize> = rows
            .iter()
            .zip(&report.entries)
            .map(|((_, text), entry)| {
                text.find(&entry.state.row_summary())
                    .expect("summary substring must be found in its own row text")
            })
            .collect();
        assert_eq!(
            starts[0], starts[1],
            "summaries must start at the same column across rows of different subject length: {rows:?}"
        );
    }

    #[test]
    fn entry_rows_empty_report_returns_empty_vec() {
        let report = PreflightReport::new();
        assert!(report.entry_rows().is_empty());
    }

    /// `InModeration`/`PRPending` are non-blocking reported states; the
    /// `Display` message must not assert a `BLOCKER` label that no longer
    /// applies.
    #[test]
    fn display_does_not_label_reported_states_as_blocker() {
        let in_moderation = PublisherState::InModeration {
            reason: "package in moderation queue".into(),
        }
        .to_string();
        let pr_pending = PublisherState::PRPending("https://example.com/pr/1".into()).to_string();

        assert!(!in_moderation.contains("BLOCKER"), "{in_moderation}");
        assert!(!pr_pending.contains("BLOCKER"), "{pr_pending}");
    }

    #[test]
    fn report_all_clean_counts_every_entry() {
        let mut report = PreflightReport::new();
        report.push(entry("cargo", PublisherState::Clean));
        report.push(entry("aur", PublisherState::Clean));

        assert_eq!(report.clean_count(), 2);
    }

    #[test]
    fn published_is_not_counted_clean() {
        let mut report = PreflightReport::new();
        report.push(entry("cargo", PublisherState::Published));

        assert_eq!(
            report.clean_count(),
            0,
            "`clean` means nothing is upstream yet; an already-published \
             version is a different state and must not inflate the count"
        );
    }
}
