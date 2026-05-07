//! Top-level `retry:` block — user-facing YAML configuration for the shared
//! retry-with-backoff machinery.
//!
//! Mirrors GoReleaser's `Project.Retry` (`pkg/config/config.go::Retry`):
//!
//! ```yaml
//! retry:
//!   attempts: 10
//!   delay: 10s
//!   max_delay: 5m
//! ```
//!
//! Defaults match GoReleaser exactly (`Retry{Attempts:10, Delay:10s, MaxDelay:5m}`)
//! so that consumers porting from GR see identical retry behaviour with the
//! same YAML.
//!
//! [`RetryConfig::to_policy`] bridges the user-facing type to
//! [`crate::retry::RetryPolicy`] which is what `retry_sync` / `retry_async`
//! consume. The conversion fixes the multiplier at 2.0 (hard-coded in
//! `RetryPolicy::delay_for`); GR also uses a fixed 2× backoff via
//! `retry.BackOffDelay`.
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
/// All fields are optional in YAML; missing fields fall back to GoReleaser's
/// defaults (10 attempts, 10s base delay, 5m cap).
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
}

impl RetryConfig {
    /// Default attempt count (matches GoReleaser `pkg/config.Retry.Attempts`).
    pub const DEFAULT_ATTEMPTS: u32 = 10;
    /// Default initial delay (matches GoReleaser `pkg/config.Retry.Delay = 10s`).
    pub const DEFAULT_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
    /// Default delay cap (matches GoReleaser `pkg/config.Retry.MaxDelay = 5m`).
    pub const DEFAULT_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

    /// Bridge to the internal [`RetryPolicy`] consumed by
    /// [`crate::retry::retry_sync`] / [`crate::retry::retry_async`].
    pub fn to_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.attempts.max(1),
            base_delay: self.delay.duration(),
            max_delay: self.max_delay.duration(),
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: Self::DEFAULT_ATTEMPTS,
            delay: HumanDuration(Self::DEFAULT_DELAY),
            max_delay: HumanDuration(Self::DEFAULT_MAX_DELAY),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_goreleaser() {
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
        };
        assert_eq!(c.to_policy().max_attempts, 1);
    }

    #[test]
    fn to_policy_preserves_custom_values() {
        let c = RetryConfig {
            attempts: 4,
            delay: HumanDuration(std::time::Duration::from_millis(250)),
            max_delay: HumanDuration(std::time::Duration::from_secs(7)),
        };
        let p = c.to_policy();
        assert_eq!(p.max_attempts, 4);
        assert_eq!(p.base_delay, std::time::Duration::from_millis(250));
        assert_eq!(p.max_delay, std::time::Duration::from_secs(7));
    }
}
