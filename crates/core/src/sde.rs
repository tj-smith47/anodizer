//! SDE-aware "now" resolver shared across stages.
//!
//! The determinism harness exports `SOURCE_DATE_EPOCH` into every child
//! build subprocess, and any timestamp that lands in a release-pipeline
//! artifact (filename, embedded date field, RPM changelog header, ...)
//! must honor that env var instead of reading wall-clock time. Stages
//! that ignored SDE silently caused harness drift before
//! `crates/core/src/context::populate_time_vars` was made SDE-aware —
//! this module is the canonical helper for the remaining writers that
//! don't have a `Context` handy or that need a direct `chrono::DateTime`
//! rather than a template-var string.
//!
//! Resolution order matches `populate_time_vars`:
//!
//! 1. `SOURCE_DATE_EPOCH` env var (seconds since epoch). Set by the
//!    determinism harness on every child subprocess and the standard
//!    reproducibility contract for upstream CI / packagers.
//! 2. `chrono::Utc::now()` — wall-clock fallback for the common
//!    non-reproducible case.
//!
//! Note that `populate_time_vars` is still the preferred path when the
//! caller has a `Context` — the template var (`Date` / `Now` / `Timestamp`)
//! is what user-supplied templates read, and routing through the context
//! keeps a single source of truth. Use `resolve_now()` when:
//!
//! - the call site has no `Context` (built-in Tera helpers registered at
//!   engine-build time);
//! - the caller needs a typed `DateTime<Utc>` for `.format(...)` rather
//!   than parsing the RFC 3339 string the context exposes.

use crate::env_source::{EnvSource, ProcessEnvSource};
use chrono::{DateTime, Utc};

/// Resolve "now" honoring `SOURCE_DATE_EPOCH` read from `env`.
///
/// Returns the SDE-derived `DateTime<Utc>` when the env var is set to a
/// valid `i64`-parseable seconds-since-epoch value, otherwise
/// `chrono::Utc::now()`. The injected [`EnvSource`] keeps tests off
/// process-env mutation; production callers should use [`resolve_now`].
pub fn resolve_now_with_env<E: EnvSource + ?Sized>(env: &E) -> DateTime<Utc> {
    env.var("SOURCE_DATE_EPOCH")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .unwrap_or_else(Utc::now)
}

/// Resolve "now" honoring `SOURCE_DATE_EPOCH` from the process environment.
///
/// Thin wrapper over [`resolve_now_with_env`] that uses [`ProcessEnvSource`].
pub fn resolve_now() -> DateTime<Utc> {
    resolve_now_with_env(&ProcessEnvSource)
}

/// Return the `SOURCE_DATE_EPOCH`-derived `DateTime<Utc>` ONLY when the
/// env var is set on `env` to a valid seconds-since-epoch value, else
/// `None`.
///
/// Distinct from [`resolve_now_with_env`], which always returns a value
/// (falling back to `Utc::now`). Use this when a call site needs to
/// behave conditionally on whether reproducibility is in effect — e.g.
/// `stage-makeself` only injects `--packaging-date` under SDE so
/// non-harness production runs keep makeself's default
/// `LC_ALL=C date` behavior.
pub fn source_date_epoch_with_env<E: EnvSource + ?Sized>(env: &E) -> Option<DateTime<Utc>> {
    env.var("SOURCE_DATE_EPOCH")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
}

/// Process-env convenience wrapper over [`source_date_epoch_with_env`].
pub fn source_date_epoch() -> Option<DateTime<Utc>> {
    source_date_epoch_with_env(&ProcessEnvSource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_source::MapEnvSource;

    #[test]
    fn resolve_now_honors_source_date_epoch() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let now = resolve_now_with_env(&env);
        assert_eq!(now.timestamp(), 1715000000);
    }

    #[test]
    fn resolve_now_ignores_malformed_sde() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "not-a-number");
        let now = resolve_now_with_env(&env);
        // Falls back to Utc::now(); assert only that it succeeded and is
        // a wall-clock value (post-2020).
        assert!(now.timestamp() > 1_577_836_800);
    }

    #[test]
    fn resolve_now_falls_back_when_unset() {
        let env = MapEnvSource::new();
        let now = resolve_now_with_env(&env);
        assert!(now.timestamp() > 1_577_836_800);
    }

    #[test]
    fn source_date_epoch_returns_some_when_set() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let dt = source_date_epoch_with_env(&env);
        assert_eq!(dt.map(|d| d.timestamp()), Some(1715000000));
    }

    #[test]
    fn source_date_epoch_returns_none_when_unset() {
        let env = MapEnvSource::new();
        assert!(source_date_epoch_with_env(&env).is_none());
    }

    #[test]
    fn source_date_epoch_returns_none_when_malformed() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "not-a-number");
        assert!(source_date_epoch_with_env(&env).is_none());
    }
}
