//! Typed result of a post-publish poll. Consumed by the release-summary
//! renderer (a separate, deferred task) — kept structurally explicit so
//! consumers can dispatch on variant rather than parsing free-form text.
//!
//! All variants are serde-friendly so they can be embedded in
//! `dist/release-summary.json` or surfaced via `--json` output without
//! bespoke conversion code.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anodizer_core::config::HumanDuration;

/// Terminal or in-flight outcome of a post-publish poll for a single
/// publisher/package/version triple.
///
/// `polled_for` is recorded for variants where the duration carries
/// diagnostic value (operator wants to know "30 minutes elapsed and
/// the package is still pending"). It is intentionally omitted from
/// `NotPolled` (nothing was waited on) and `Approved` / `Rejected` /
/// `Error` (the duration is incidental to the result).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PostPublishStatus {
    /// Polling not attempted — either disabled per-publisher (config),
    /// disabled globally (`--no-post-publish-poll`), or the publisher
    /// doesn't implement a poller. Distinguished from `Pending` so
    /// summaries can render "not checked" rather than misleading the
    /// operator into thinking the queue is empty.
    NotPolled,
    /// Polled — observed an in-flight state at the last sample. Always
    /// paired with a `Timeout` if the polling budget elapsed without
    /// reaching a terminal state.
    Pending {
        /// Short, human-readable diagnostic (e.g. "awaiting moderation",
        /// "validation in progress").
        detail: String,
        /// Wall-clock duration spent polling before this result was
        /// captured.
        polled_for: HumanDuration,
    },
    /// Polled — observed a terminal-success state (Chocolatey
    /// `callout-header: Package Approved`, WinGet PR `merged: true` or
    /// `Moderator-Approved` label).
    Approved {
        /// Short, human-readable diagnostic (e.g. "PR merged", "approved").
        detail: String,
    },
    /// Polled — observed a terminal-failure state (Chocolatey moderator
    /// rejected, WinGet `Validation-*-Error` / `merged: false` / closed
    /// without merge).
    Rejected {
        /// Short, human-readable diagnostic — name the specific failure
        /// label or reason ("Validation-Installation-Error", "PR closed
        /// without merge", ...).
        detail: String,
    },
    /// Polling exhausted its budget without reaching a terminal state.
    /// Records the last observed state so the summary can report
    /// "still pending after 30m of polling — manual follow-up required".
    Timeout {
        /// Last in-flight diagnostic observed before the budget expired.
        last_state: String,
        /// Total wall-clock duration the poller waited.
        polled_for: HumanDuration,
    },
    /// Polling failed unrecoverably (network blackhole, parse error,
    /// auth refusal). The polling result is advisory only — the publish
    /// step has already succeeded.
    Error {
        /// Short, human-readable diagnostic — preserve the underlying
        /// error chain ("HTTP 503 after 3 retries", "malformed HTML",
        /// "GitHub API auth failed").
        reason: String,
    },
}

impl PostPublishStatus {
    /// Helper for poller implementations: build a `Pending` variant
    /// from raw fields without forcing every callsite to repeat the
    /// `HumanDuration` wrapping.
    pub fn pending(detail: impl Into<String>, polled_for: Duration) -> Self {
        Self::Pending {
            detail: detail.into(),
            polled_for: HumanDuration(polled_for),
        }
    }

    /// Helper for poller implementations: build a `Timeout` variant
    /// from raw fields.
    pub fn timeout(last_state: impl Into<String>, polled_for: Duration) -> Self {
        Self::Timeout {
            last_state: last_state.into(),
            polled_for: HumanDuration(polled_for),
        }
    }

    /// True for terminal states (`Approved`, `Rejected`, `Timeout`,
    /// `Error`) — the poller is done and won't change. `NotPolled` is
    /// also terminal (no poll happens). `Pending` is non-terminal.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending { .. })
    }
}

/// Per-publisher post-publish polling outcome. The publisher name +
/// package + version + status quad is the unit consumed by the release
/// summary; keeping them grouped here avoids the summary code needing
/// to thread three identifiers + a status enum through every helper.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PostPublishResult {
    /// Publisher label — `"chocolatey"`, `"winget"`. Matches the label
    /// the publish stage uses elsewhere.
    pub publisher: String,
    /// Package name as submitted (Chocolatey package id / WinGet
    /// PackageIdentifier).
    pub package: String,
    /// Version string as submitted.
    pub version: String,
    /// Polling outcome.
    pub status: PostPublishStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_constructor_wraps_duration() {
        let s = PostPublishStatus::pending("awaiting moderation", Duration::from_secs(45));
        match s {
            PostPublishStatus::Pending { detail, polled_for } => {
                assert_eq!(detail, "awaiting moderation");
                assert_eq!(polled_for.duration(), Duration::from_secs(45));
            }
            other => panic!("expected Pending, got {:?}", other),
        }
    }

    #[test]
    fn timeout_constructor_wraps_duration() {
        let s = PostPublishStatus::timeout("validation in progress", Duration::from_secs(1800));
        match s {
            PostPublishStatus::Timeout {
                last_state,
                polled_for,
            } => {
                assert_eq!(last_state, "validation in progress");
                assert_eq!(polled_for.duration(), Duration::from_secs(1800));
            }
            other => panic!("expected Timeout, got {:?}", other),
        }
    }

    #[test]
    fn is_terminal_classification() {
        assert!(PostPublishStatus::NotPolled.is_terminal());
        assert!(!PostPublishStatus::pending("x", Duration::from_secs(1)).is_terminal());
        assert!(
            PostPublishStatus::Approved {
                detail: "ok".into()
            }
            .is_terminal()
        );
        assert!(
            PostPublishStatus::Rejected {
                detail: "bad".into()
            }
            .is_terminal()
        );
        assert!(PostPublishStatus::timeout("x", Duration::from_secs(1)).is_terminal());
        assert!(PostPublishStatus::Error { reason: "x".into() }.is_terminal());
    }

    #[test]
    fn serializes_as_tagged_enum() {
        // Confirm the JSON-schema-friendly shape so the deferred Q5
        // summary renderer can dispatch on `kind` directly.
        let pending = PostPublishStatus::pending("queued", Duration::from_secs(30));
        let json = serde_json::to_string(&pending).unwrap();
        assert!(json.contains(r#""kind":"pending""#), "got: {json}");
        assert!(json.contains(r#""detail":"queued""#), "got: {json}");

        let approved = PostPublishStatus::Approved {
            detail: "merged".into(),
        };
        let json = serde_json::to_string(&approved).unwrap();
        assert!(json.contains(r#""kind":"approved""#), "got: {json}");
    }
}
