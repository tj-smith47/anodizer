//! Post-publish polling configuration shared by the Chocolatey and WinGet
//! publishers.
//!
//! Both publishers report `HTTP 2xx` from the submission endpoint long
//! before the upstream actually approves the package (Chocolatey
//! moderation queue) or merges the PR (winget-pkgs validation pipeline).
//! When polling is enabled, the publish stage waits for a terminal
//! moderation/validation state up to `timeout`, sampling every
//! `interval`, and surfaces the result as part of the release summary.
//!
//! Defaults: `enabled: false`, `interval: 30s`, `timeout: 30m`. The
//! human-moderation queues these publishers feed routinely take
//! HOURS to DAYS to clear; blocking a CI job for that long is wrong by
//! default. Operators who genuinely want in-band verification opt in
//! per-publisher with `post_publish_poll: { enabled: true }`. Operators
//! who want global opt-out (e.g., already opted-in via top-level
//! config) keep `--no-post-publish-poll` as the override.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::HumanDuration;

/// Per-publisher post-publish polling config block.
///
/// See module-level docs for the polling lifecycle. Default values:
/// `enabled: true`, `interval: 30s`, `timeout: 30m`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PostPublishPollConfig {
    /// Whether to poll at all. Default `false` — the upstream moderation
    /// queues these publishers feed (Chocolatey, winget-pkgs) routinely
    /// take HOURS to DAYS; the publish stage cannot reasonably block on
    /// them in a CI workflow. Opt in to in-band verification by setting
    /// `true` per-publisher (e.g. when running locally and willing to
    /// wait).
    pub enabled: bool,
    /// How long to wait between successive status checks. Default `30s`.
    pub interval: HumanDuration,
    /// Total wall-clock budget for polling. When exhausted, the poller
    /// emits `PostPublishStatus::Timeout` with the last observed state.
    /// Default `30m`.
    pub timeout: HumanDuration,
}

impl PostPublishPollConfig {
    /// Default interval between successive polls (30 seconds).
    pub const DEFAULT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    /// Default total polling budget (30 minutes).
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
}

impl Default for PostPublishPollConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: HumanDuration(Self::DEFAULT_INTERVAL),
            timeout: HumanDuration(Self::DEFAULT_TIMEOUT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = PostPublishPollConfig::default();
        assert!(!c.enabled, "polling is opt-in — defaults to disabled");
        assert_eq!(c.interval.duration(), std::time::Duration::from_secs(30));
        assert_eq!(
            c.timeout.duration(),
            std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn empty_yaml_yields_defaults() {
        let c: PostPublishPollConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert!(!c.enabled);
        assert_eq!(c.interval.duration(), std::time::Duration::from_secs(30));
        assert_eq!(
            c.timeout.duration(),
            std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn parses_explicit_yaml() {
        let yaml = "enabled: false\ninterval: 1m\ntimeout: 5m\n";
        let c: PostPublishPollConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.interval.duration(), std::time::Duration::from_secs(60));
        assert_eq!(c.timeout.duration(), std::time::Duration::from_secs(5 * 60));
    }

    #[test]
    fn unknown_field_rejected() {
        let yaml = "interval: 1m\nbogus: true\n";
        let res: Result<PostPublishPollConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err(), "deny_unknown_fields must reject typos");
    }
}
