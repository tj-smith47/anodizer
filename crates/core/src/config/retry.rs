//! Top-level `retry:` block — user-facing YAML configuration for the shared
//! retry-with-backoff machinery.
//!
//! Project-level retry policy:
//!
//! ```yaml
//! retry:
//!   attempts: 10
//!   delay: 10s
//!   max_delay: 5m
//! ```
//!
//! Defaults are 10 attempts, a 10s first delay, a 5m cap on each delay and
//! a 15-minute retry budget per publisher run or release run (`max_elapsed`).
//!
//! [`RetryConfig::to_policy`] bridges the user-facing type to
//! [`crate::retry::RetryPolicy`] which is what `retry_sync` / `retry_async`
//! consume. The conversion fixes the multiplier at 2.0 (hard-coded in
//! `RetryPolicy::delay_for`).
//!
//! ## See also
//!
//! - [`crate::retry`] — the policy + retry primitives.
//! - [`crate::retry::is_retriable`] — companion predicate (network / 5xx /
//!   429 / explicitly-marked retriable).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::HumanDuration;
use crate::retry::RetryPolicy;

/// User-facing retry configuration block (`retry:` at config root).
///
/// All fields are optional in YAML; missing fields fall back to the
/// defaults (10 attempts, 10s base delay, 5m cap on each delay, and a
/// 15-minute `max_elapsed` budget for each publisher run).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    /// Total attempts (including the first). Default `10`. Values < 1 are
    /// clamped up to 1 by the policy layer.
    pub attempts: u32,
    /// Initial delay before the second attempt. Default `10s`. Subsequent
    /// delays grow exponentially (`delay × 2^(n-2)`) up to [`Self::max_delay`].
    pub delay: HumanDuration,
    /// Upper bound on any individual sleep between attempts. Default `5m`.
    /// Without this cap, an exponential backoff with `delay=10s` would
    /// stretch attempt 9 to ~42 minutes.
    pub max_delay: HumanDuration,
    /// Cap on total retry wall-time. Each publisher run gets its own
    /// deadline: the moment the publisher starts plus this budget. Every
    /// retried request inside that run (an auth exchange, the publish
    /// request, a cleanup) shares that one deadline, and a second publisher
    /// starts a fresh one. A GitHub, GitLab or Gitea release run takes its
    /// deadline once when it starts, shared by its API calls and asset
    /// uploads. A retry outside both starts its own deadline when it begins.
    ///
    /// Retrying stops before a backoff sleep would pass the deadline, so a
    /// long transient storm fails with the last error, and an idempotent
    /// re-run can resume. When unset the budget is 15 minutes. A release
    /// whose CI job has a hard timeout should keep it below that timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_elapsed: Option<HumanDuration>,
}

impl RetryConfig {
    /// Default attempt count (10).
    pub const DEFAULT_ATTEMPTS: u32 = 10;
    /// Default initial delay (10s).
    pub const DEFAULT_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
    /// Default delay cap (5m).
    pub const DEFAULT_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

    /// Bridge to the internal [`RetryPolicy`] consumed by
    /// [`crate::retry::retry_sync`] / [`crate::retry::retry_async`].
    ///
    /// If `max_delay < delay`, every backoff is immediately capped to
    /// `max_delay`. The values pass through unchanged, but the pair is
    /// almost certainly a config mistake, so a `tracing::warn!` fires once
    /// at conversion time.
    pub fn to_policy(&self) -> RetryPolicy {
        if self.max_delay.duration() < self.delay.duration() {
            tracing::warn!(
                "retry.max_delay ({:?}) is less than retry.delay ({:?}); \
                 backoff will be capped at max_delay",
                self.max_delay.duration(),
                self.delay.duration(),
            );
        }
        RetryPolicy {
            max_attempts: self.attempts.max(1),
            base_delay: self.delay.duration(),
            max_delay: self.max_delay.duration(),
        }
    }

    /// The configured total-retry-wall-time budget, if any.
    pub fn max_elapsed_duration(&self) -> Option<std::time::Duration> {
        self.max_elapsed.map(|h| h.duration())
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: Self::DEFAULT_ATTEMPTS,
            delay: HumanDuration(Self::DEFAULT_DELAY),
            max_delay: HumanDuration(Self::DEFAULT_MAX_DELAY),
            max_elapsed: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_ten_attempts_ten_seconds_and_five_minutes() {
        let c = RetryConfig::default();
        assert_eq!(c.attempts, 10);
        assert_eq!(c.delay.duration(), std::time::Duration::from_secs(10));
        assert_eq!(c.max_delay.duration(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn empty_yaml_yields_defaults() {
        let c: RetryConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(c.attempts, 10);
        assert_eq!(c.delay.duration(), std::time::Duration::from_secs(10));
        assert_eq!(c.max_delay.duration(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn parses_explicit_yaml() {
        let yaml = r#"
attempts: 5
delay: 1s
max_delay: 30s
"#;
        let c: RetryConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(c.attempts, 5);
        assert_eq!(c.delay.duration(), std::time::Duration::from_secs(1));
        assert_eq!(c.max_delay.duration(), std::time::Duration::from_secs(30));
    }

    #[test]
    fn parses_compound_humantime() {
        let yaml = r#"
attempts: 3
delay: 500ms
max_delay: 1h30m
"#;
        let c: RetryConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(c.delay.duration(), std::time::Duration::from_millis(500));
        assert_eq!(
            c.max_delay.duration(),
            std::time::Duration::from_secs(90 * 60),
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "bogus: 1";
        let result: Result<RetryConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err(), "expected deny_unknown_fields to reject");
    }

    #[test]
    fn to_policy_round_trip_defaults() {
        let policy = RetryConfig::default().to_policy();
        assert_eq!(policy.max_attempts, 10);
        assert_eq!(policy.base_delay, std::time::Duration::from_secs(10));
        assert_eq!(policy.max_delay, std::time::Duration::from_secs(300));
    }

    #[test]
    fn to_policy_clamps_zero_attempts_to_one() {
        let c = RetryConfig {
            attempts: 0,
            delay: HumanDuration(std::time::Duration::from_secs(1)),
            max_delay: HumanDuration(std::time::Duration::from_secs(2)),
            max_elapsed: None,
        };
        assert_eq!(c.to_policy().max_attempts, 1);
    }

    #[test]
    fn to_policy_max_delay_below_delay_does_not_panic() {
        // A max_delay below delay is kept as written, since a config that
        // loads must not panic at conversion; the warning names the mistake.
        let c = RetryConfig {
            attempts: 3,
            delay: HumanDuration(std::time::Duration::from_secs(10)),
            max_delay: HumanDuration(std::time::Duration::from_secs(1)),
            max_elapsed: None,
        };
        let p = c.to_policy();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.base_delay, std::time::Duration::from_secs(10));
        assert_eq!(p.max_delay, std::time::Duration::from_secs(1));
    }

    #[test]
    fn to_policy_preserves_custom_values() {
        let c = RetryConfig {
            attempts: 4,
            delay: HumanDuration(std::time::Duration::from_millis(250)),
            max_delay: HumanDuration(std::time::Duration::from_secs(7)),
            max_elapsed: None,
        };
        let p = c.to_policy();
        assert_eq!(p.max_attempts, 4);
        assert_eq!(p.base_delay, std::time::Duration::from_millis(250));
        assert_eq!(p.max_delay, std::time::Duration::from_secs(7));
    }

    #[test]
    fn max_elapsed_defaults_to_none() {
        let c = RetryConfig::default();
        assert_eq!(c.max_elapsed, None);
        assert_eq!(c.max_elapsed_duration(), None);
    }

    #[test]
    fn max_elapsed_parses_and_other_fields_default() {
        let c: RetryConfig = serde_yaml_ng::from_str("max_elapsed: 15m").unwrap();
        assert_eq!(
            c.max_elapsed_duration(),
            Some(std::time::Duration::from_secs(15 * 60))
        );
        assert_eq!(c.attempts, 10);
        assert_eq!(c.delay.duration(), std::time::Duration::from_secs(10));
        assert_eq!(c.max_delay.duration(), std::time::Duration::from_secs(300));
    }

    #[test]
    fn max_elapsed_unset_leaves_accessor_none() {
        let c: RetryConfig = serde_yaml_ng::from_str("attempts: 5").unwrap();
        assert_eq!(c.max_elapsed_duration(), None);
    }
}
