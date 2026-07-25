//! The `anodizer preflight` reconcile table: each SELECTED publisher's answer
//! to "am I already done for this exact version+content?".
//!
//! Two halves make the canary agree with the release it guards, both enforced
//! here rather than at the call site:
//!
//! * the probe is [`Publisher::reconcile`] — the same call the publish
//!   dispatch loop makes, so the verdicts cannot come from two drifting
//!   implementations;
//! * the publisher list is [`crate::registry::selected_publishers`], the same
//!   `--skip` / `--publishers` selection the dispatch loop applies before it
//!   reconciles. [`ReconcileReport::probe`] derives that list itself and takes
//!   no publisher argument, so a caller cannot hand it an unfiltered set and
//!   gate a run on a registry the run would never write to.
//!
//! # Exit contract
//!
//! Only [`ReconcileState::Diverged`] blocks. `Complete` is the green light a
//! resumed release wants (the version is already upstream with matching
//! bytes), `Absent` is the ordinary pre-publish state, and `Unknown` never
//! blocks — an unreachable registry must not stand between an operator and a
//! release, since the registry's own conflict handling is the backstop.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::preflight::RowKind;
use anodizer_core::{Publisher, ReconcileState};
use serde::Serialize;

/// One publisher's reconcile result.
#[derive(Debug, Clone)]
pub struct ReconcileRow {
    /// Publisher name as it appears in config and logs (`cargo`, `winget`, …).
    pub publisher: String,
    /// What the probe concluded, or the error that prevented a conclusion.
    /// A probe that returns `Err` is rendered — and treated — exactly as
    /// [`ReconcileState::Unknown`]: reported, never blocking.
    pub state: ReconcileState,
    /// [`Publisher::required`], carried so a divergence blocks exactly when
    /// the release itself would abort on it.
    pub required: bool,
}

/// Serializable projection of a [`ReconcileRow`] for `--json` consumers.
#[derive(Debug, Serialize)]
pub struct ReconcileRowJson {
    /// Publisher name, or [`WHOLE_SWEEP`] on the single marker row a skipped
    /// sweep emits.
    pub publisher: String,
    /// Lowercase state discriminant: `absent`, `complete`, `diverged`,
    /// `unknown`, or `skipped` (the whole-sweep marker).
    pub state: &'static str,
    /// The state's payload (`note` / `detail` / `reason`); absent for
    /// `Absent`, which carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether this row alone makes the command exit non-zero.
    pub blocking: bool,
}

/// The `publisher` field of the marker row a skipped sweep emits: the
/// whole-set wildcard, since the row describes every publisher at once rather
/// than any single one.
pub const WHOLE_SWEEP: &str = "*";

/// Each selected publisher's reconcile state for the target version, or the
/// reason the sweep did not apply at all.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub rows: Vec<ReconcileRow>,
    /// Why no publisher was probed. `None` for a sweep that ran (`rows` is
    /// then the complete answer); `Some(reason)` when the question itself did
    /// not apply to this tree, which is NOT the same fact as "no publisher is
    /// configured" and must not be reported as an empty table.
    pub skipped: Option<String>,
}

impl ReconcileReport {
    /// Probe every publisher this run would dispatch, deriving the list from
    /// [`crate::registry::selected_publishers`] so `--skip` / `--publishers`
    /// scope the sweep exactly as they scope the publish loop.
    ///
    /// A publisher whose probe errors is recorded as `Unknown` rather than
    /// aborting the sweep, so one unreachable registry cannot hide every other
    /// publisher's state.
    pub fn probe(ctx: &mut Context) -> Self {
        // Built from a shared borrow up front so the probe loop below can hold
        // the `&mut Context` each `reconcile` call needs.
        let publishers = crate::registry::selected_publishers(ctx);
        Self::probe_selected(&publishers, ctx)
    }

    /// A report standing in for a sweep that does not apply to this tree,
    /// carrying the operator-facing `reason` in place of the table.
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            rows: Vec::new(),
            skipped: Some(reason.into()),
        }
    }

    /// [`ReconcileReport::probe`]'s body, over an explicit publisher list.
    ///
    /// Re-applies the deselection predicate rather than trusting the list to
    /// be pre-filtered: the invariant "no deselected publisher is ever probed"
    /// then holds for every path into the sweep, not only the one that sourced
    /// its list from the registry.
    pub(crate) fn probe_selected(publishers: &[Box<dyn Publisher>], ctx: &mut Context) -> Self {
        let mut rows = Vec::new();
        for p in publishers {
            if ctx.publisher_deselected(p.name()) {
                continue;
            }
            let probed = {
                let _scope = anodizer_core::retry::PublisherRetryScope::enter(p.name());
                p.reconcile(ctx)
            };
            let state = probed.unwrap_or_else(|err| ReconcileState::Unknown {
                reason: format!("probe failed: {err:#}"),
            });
            rows.push(ReconcileRow {
                publisher: p.name().to_string(),
                required: p.required(),
                state,
            });
        }
        Self {
            rows,
            skipped: None,
        }
    }

    /// Every publisher whose local bytes differ from what is upstream,
    /// blocking or not.
    pub fn diverged(&self) -> Vec<&ReconcileRow> {
        self.rows
            .iter()
            .filter(|r| matches!(r.state, ReconcileState::Diverged { .. }))
            .collect()
    }

    /// Rows that make the command exit non-zero: a divergence on a REQUIRED
    /// publisher.
    ///
    /// An optional publisher's divergence is reported but does not block,
    /// mirroring the dispatch arm (`Diverged if p.required()`) — a canary
    /// stricter than the release it guards would fail runs the release would
    /// have completed.
    pub fn blocking(&self) -> Vec<&ReconcileRow> {
        self.diverged().into_iter().filter(|r| r.required).collect()
    }

    /// One `(kind, text)` row per publisher, ready for `StageLogger` dispatch
    /// — or the single indented marker line when the sweep was skipped, so no
    /// rendering path can drop that state and show an empty table instead.
    ///
    /// Publisher names are padded to the widest present so the summaries line
    /// up in a column, matching `PreflightReport::entry_rows`.
    pub fn entry_rows(&self) -> Vec<(RowKind, String)> {
        if let Some(reason) = &self.skipped {
            return vec![(RowKind::Info, format!("  skipped — {reason}"))];
        }
        let width = self
            .rows
            .iter()
            .map(|r| r.publisher.len())
            .max()
            .unwrap_or(0);
        self.rows
            .iter()
            .map(|row| {
                let name = &row.publisher;
                (
                    row_kind(&row.state),
                    format!("{name:width$}  {}", row_summary(&row.state)),
                )
            })
            .collect()
    }

    /// Render this report through `log`, one `StageLogger` call per row.
    pub fn emit(&self, log: &StageLogger) {
        log.status("Reconcile state");
        for (kind, text) in self.entry_rows() {
            match kind {
                RowKind::Ok => log.success(&text),
                RowKind::Info => log.status(&text),
            }
        }
        // The row above already carries the full divergence detail; these
        // lines exist to state the action, so repeating the detail would bury
        // it. An optional publisher's divergence is a warning because it will
        // not abort the release either.
        for row in self.diverged() {
            let text = format!(
                "{}: bump the version and re-run — published content is immutable",
                row.publisher
            );
            if row.required {
                log.error(&text);
            } else {
                log.warn(&text);
            }
        }
    }

    /// `--json` projection.
    ///
    /// A skipped sweep projects to ONE marker row rather than to `[]`, so a
    /// consumer reading the array cannot mistake "this question did not apply"
    /// for "nothing is configured" — two states with opposite meanings that an
    /// empty array renders identically.
    pub fn to_json_rows(&self) -> Vec<ReconcileRowJson> {
        if let Some(reason) = &self.skipped {
            return vec![ReconcileRowJson {
                publisher: WHOLE_SWEEP.to_string(),
                state: "skipped",
                detail: Some(reason.clone()),
                blocking: false,
            }];
        }
        self.rows
            .iter()
            .map(|row| ReconcileRowJson {
                publisher: row.publisher.clone(),
                state: state_tag(&row.state),
                detail: state_detail(&row.state),
                blocking: row.required && matches!(row.state, ReconcileState::Diverged { .. }),
            })
            .collect()
    }
}

/// `Complete` is the only state that earns the `✓` success marker: it is the
/// sole positive confirmation that the target is already live upstream.
/// `Diverged` gets `•` here because its blocking message is emitted
/// separately through the logger's `Error` label.
fn row_kind(state: &ReconcileState) -> RowKind {
    match state {
        ReconcileState::Complete { .. } => RowKind::Ok,
        ReconcileState::Absent
        | ReconcileState::Diverged { .. }
        | ReconcileState::Unknown { .. } => RowKind::Info,
    }
}

fn row_summary(state: &ReconcileState) -> String {
    match state {
        ReconcileState::Absent => "absent — will publish".to_string(),
        ReconcileState::Complete { note } => format!("complete — {note}"),
        ReconcileState::Diverged { detail } => format!("diverged — {detail}"),
        ReconcileState::Unknown { reason } => format!("unknown — {reason}"),
    }
}

fn state_tag(state: &ReconcileState) -> &'static str {
    match state {
        ReconcileState::Absent => "absent",
        ReconcileState::Complete { .. } => "complete",
        ReconcileState::Diverged { .. } => "diverged",
        ReconcileState::Unknown { .. } => "unknown",
    }
}

fn state_detail(state: &ReconcileState) -> Option<String> {
    match state {
        ReconcileState::Absent => None,
        ReconcileState::Complete { note } => Some(note.clone()),
        ReconcileState::Diverged { detail } => Some(detail.clone()),
        ReconcileState::Unknown { reason } => Some(reason.clone()),
    }
}

#[cfg(test)]
mod tests;
