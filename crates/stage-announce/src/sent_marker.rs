//! Per-version announce sent-marker — makes re-runs idempotent.
//!
//! Announcer channels (Discord, Slack, Twitter, email, …) are fire-and-forget:
//! posting a message has no version-keyed query the publisher could consult to
//! ask "did I already announce this release?". Re-running a release at a
//! version that already announced would therefore re-post to every channel.
//!
//! This marker closes that gap channel-agnostically. After an announcer's
//! `send` succeeds, its name is recorded in
//! `<dist>/.announce-sent-<version>.json`. On a later run at the same version,
//! any announcer already listed is skipped.
//!
//! **Delivery guarantee (honest):** the marker gives *exactly-once* for any
//! send that **completed and was drained** before the announce stage's
//! aggregate deadline — its success is recorded, so a re-run skips it. It
//! degrades to *at-least-once* only for a send still **genuinely in-flight**
//! when the deadline elapsed: that straggler is abandoned without a marker, so
//! a re-run re-fires it (and it may also still land on the first run, hence the
//! possible duplicate). A send that *finished* right at the deadline is NOT a
//! straggler — the dispatch runner does a final non-blocking drain of completed
//! results before computing the abandoned set, so a completed-but-undrained
//! send is recorded rather than re-fired. The residual duplicate window is
//! therefore confined to sends that had not returned by the deadline.
//!
//! The marker is keyed by VERSION (not run-id): a recovery re-run gets a fresh
//! run-id, so a run-id-keyed marker could never deduplicate across runs. It is
//! best-effort persistence — a read/parse failure degrades to "nothing
//! recorded yet" (the announcer sends, exactly as it would have without the
//! marker), and a write failure is logged but never fails the release, because
//! a missing dedup record must not abort a publish.
//!
//! **Cross-run scope:** dedup is effective only when the same dist directory is
//! reused across re-runs (e.g. via `skip-determinism: true`). If the dist tree
//! is rebuilt from scratch in a fresh CI run, the marker file is absent and all
//! announcers fire again. This is intentional: a rebuilt dist might contain
//! different release notes or assets, so re-announcing is the safe default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anodizer_core::log::StageLogger;
use serde::{Deserialize, Serialize};

/// On-disk shape of the marker file. A sorted set of announcer names that have
/// already fired for the marker's version, plus the version itself for
/// human-readability / debugging.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SentRecord {
    version: String,
    sent: BTreeSet<String>,
}

/// Per-version announce sent-marker, loaded from (and flushed to)
/// `<dist>/.announce-sent-<version>.json`.
pub(crate) struct AnnounceSentMarker {
    path: PathBuf,
    record: SentRecord,
}

/// Sanitize a version string into a filesystem-safe marker filename component.
///
/// Versions are normally semver (`1.2.3`, `1.2.3-rc.1+build`), but build
/// metadata can carry `+` and a templated version could in theory carry a path
/// separator. Replace anything outside `[A-Za-z0-9._-]` with `_` so the marker
/// can never escape `<dist>/` or collide with a directory separator.
fn sanitize_version(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl AnnounceSentMarker {
    /// Build the marker path for `version` under `dist`.
    fn path_for(dist: &Path, version: &str) -> PathBuf {
        dist.join(format!(".announce-sent-{}.json", sanitize_version(version)))
    }

    /// Load the marker for `version` from `dist`, or an empty marker when no
    /// file exists yet / the file can't be parsed (best-effort: a corrupt or
    /// absent marker means "nothing sent yet", so announcers run).
    pub(crate) fn load(dist: &Path, version: &str, log: &StageLogger) -> Self {
        let path = Self::path_for(dist, version);
        let record = match std::fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<SentRecord>(&body) {
                Ok(r) => r,
                Err(e) => {
                    log.debug(&format!(
                        "ignoring unreadable sent-marker {}: {e}",
                        path.display()
                    ));
                    SentRecord {
                        version: version.to_string(),
                        sent: BTreeSet::new(),
                    }
                }
            },
            // Missing file is the common first-run case — not an error.
            Err(_) => SentRecord {
                version: version.to_string(),
                sent: BTreeSet::new(),
            },
        };
        Self { path, record }
    }

    /// Whether `announcer` already fired for this version on a prior run.
    pub(crate) fn already_sent(&self, announcer: &str) -> bool {
        self.record.sent.contains(announcer)
    }

    /// Record that `announcer` fired for this version and flush to disk.
    ///
    /// Flushed eagerly per announcer (rather than once at the end) so a
    /// mid-dispatch crash after some channels posted still records exactly
    /// those channels — the next re-run skips them and only sends the
    /// remaining ones. A write failure is logged, never propagated.
    pub(crate) fn mark_sent(&mut self, announcer: &str, log: &StageLogger) {
        if !self.record.sent.insert(announcer.to_string()) {
            return;
        }
        self.flush(log);
    }

    fn flush(&self, log: &StageLogger) {
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            log.warn(&format!(
                "could not create dir for sent-marker {}: {e}",
                self.path.display()
            ));
            return;
        }
        match serde_json::to_string_pretty(&self.record) {
            Ok(body) => {
                if let Err(e) = std::fs::write(&self.path, body) {
                    log.warn(&format!(
                        "could not write sent-marker {}: {e}",
                        self.path.display()
                    ));
                }
            }
            Err(e) => log.warn(&format!("could not serialize sent-marker: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::Verbosity;

    fn quiet() -> StageLogger {
        StageLogger::new("announce", Verbosity::Quiet)
    }

    #[test]
    fn absent_marker_reports_nothing_sent() {
        let dir = tempfile::tempdir().unwrap();
        let m = AnnounceSentMarker::load(dir.path(), "1.0.0", &quiet());
        assert!(!m.already_sent("discord"));
    }

    #[test]
    fn mark_then_reload_is_idempotent_per_announcer() {
        let dir = tempfile::tempdir().unwrap();
        let log = quiet();
        {
            let mut m = AnnounceSentMarker::load(dir.path(), "1.0.0", &log);
            m.mark_sent("discord", &log);
            m.mark_sent("slack", &log);
        }
        // A fresh load (simulating a re-run) sees the persisted set.
        let reloaded = AnnounceSentMarker::load(dir.path(), "1.0.0", &log);
        assert!(reloaded.already_sent("discord"));
        assert!(reloaded.already_sent("slack"));
        assert!(!reloaded.already_sent("twitter"));
    }

    #[test]
    fn marker_is_keyed_per_version() {
        let dir = tempfile::tempdir().unwrap();
        let log = quiet();
        {
            let mut m = AnnounceSentMarker::load(dir.path(), "1.0.0", &log);
            m.mark_sent("discord", &log);
        }
        // A different version starts clean — a new release re-announces.
        let other = AnnounceSentMarker::load(dir.path(), "2.0.0", &log);
        assert!(!other.already_sent("discord"));
    }

    #[test]
    fn version_with_path_chars_cannot_escape_dist() {
        let dir = tempfile::tempdir().unwrap();
        let log = quiet();
        let mut m = AnnounceSentMarker::load(dir.path(), "1.0.0+meta/../../etc", &log);
        m.mark_sent("discord", &log);
        // The marker file lands directly under dist, not in a parent dir.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one marker under dist: {entries:?}"
        );
        assert!(entries[0].starts_with(".announce-sent-"));
        assert!(!entries[0].contains('/'));
    }

    #[test]
    fn corrupt_marker_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let log = quiet();
        let path = AnnounceSentMarker::path_for(dir.path(), "1.0.0");
        std::fs::write(&path, b"not json at all").unwrap();
        let m = AnnounceSentMarker::load(dir.path(), "1.0.0", &log);
        // Unparseable → treated as "nothing sent", so announcers still run.
        assert!(!m.already_sent("discord"));
    }
}
