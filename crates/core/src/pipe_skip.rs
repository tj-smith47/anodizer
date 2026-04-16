//! Aggregated per-stage skip tracking.
//!
//! Mirrors GoReleaser's `internal/pipe/pipe.go::SkipMemento`. Pipeline
//! stages that iterate multiple sub-configs (signs, docker_signs, custom
//! publishers, archives, nfpms, …) occasionally need to skip a sub-config
//! for a legitimate reason: `artifacts: none`, an `if:` conditional that
//! rendered to `"false"`, an `ids` filter that matched nothing, an empty
//! `cmd`. Before this module those skips used a bare `continue;` — the
//! end-of-pipeline summary lost all visibility into intentional skips, so a
//! misconfigured sign block and a deliberately-disabled one looked
//! identical in the logs.
//!
//! `SkipMemento` collects a (stage, config_label, reason) tuple per skip.
//! The pipeline runner drains it at end-of-pipeline and prints a grouped
//! summary so users know which sub-configs were intentionally skipped.

use std::sync::{Arc, Mutex};

/// A single skip event: which stage, which sub-config, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipEvent {
    /// Stage that raised the skip (`sign`, `docker-sign`, `publisher`, …).
    pub stage: String,
    /// Human-readable label for the sub-config (e.g. the sign config's `id`,
    /// the publisher's `name`, or a positional `publisher[2]`).
    pub label: String,
    /// Reason the skip happened (short, single-line).
    pub reason: String,
}

/// Thread-safe aggregator. Cheap to clone (wraps `Arc<Mutex<…>>`).
///
/// Stages record via `remember`. The pipeline runner calls `drain` at
/// end-of-pipeline to print the summary. Duplicate `(stage, label, reason)`
/// tuples are dropped on insert so a per-artifact inner `continue` doesn't
/// emit N copies of the same skip message.
#[derive(Debug, Clone, Default)]
pub struct SkipMemento {
    inner: Arc<Mutex<Vec<SkipEvent>>>,
}

impl SkipMemento {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a skip. Duplicate `(stage, label, reason)` tuples are dropped.
    pub fn remember(&self, stage: &str, label: &str, reason: &str) {
        let event = SkipEvent {
            stage: stage.to_string(),
            label: label.to_string(),
            reason: reason.to_string(),
        };
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.iter().any(|e| e == &event) {
            guard.push(event);
        }
    }

    /// Current number of recorded skips. Useful for tests and the summary
    /// header (`"3 intentional skips"`).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take a snapshot of recorded skips without clearing. Used by tests.
    pub fn snapshot(&self) -> Vec<SkipEvent> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Drain all recorded skips, leaving the memento empty.
    pub fn drain(&self) -> Vec<SkipEvent> {
        self.inner
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_drains() {
        let m = SkipMemento::new();
        assert!(m.is_empty());
        m.remember("sign", "cosign", "artifacts: none");
        m.remember("sign", "gpg", "if: false");
        assert_eq!(m.len(), 2);

        let drained = m.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].stage, "sign");
        assert_eq!(drained[0].label, "cosign");
        assert_eq!(drained[0].reason, "artifacts: none");
        assert!(m.is_empty());
    }

    #[test]
    fn deduplicates_identical_events() {
        let m = SkipMemento::new();
        // A per-artifact inner loop may fire "ids filter" skip N times for
        // the same sign config; only one summary line should survive.
        for _ in 0..10 {
            m.remember("sign", "cosign", "ids filter matched no artifacts");
        }
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn keeps_distinct_reasons_per_label() {
        let m = SkipMemento::new();
        m.remember("sign", "cosign", "artifacts: none");
        m.remember("sign", "cosign", "if: false");
        // Same label, different reasons → both survive so the user sees
        // each distinct skip path.
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn snapshot_does_not_clear() {
        let m = SkipMemento::new();
        m.remember("publisher", "my-tool", "empty cmd");
        let snap = m.snapshot();
        assert_eq!(snap.len(), 1);
        // Still present after snapshot.
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn clone_shares_state() {
        let m = SkipMemento::new();
        let m2 = m.clone();
        m2.remember("docker-sign", "cosign-docker", "artifacts: none");
        assert_eq!(m.len(), 1);
        assert_eq!(m.snapshot(), m2.snapshot());
    }
}
