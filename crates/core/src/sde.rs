//! SDE-aware "now" resolver shared across stages.
//!
//! The determinism harness exports `SOURCE_DATE_EPOCH` into every child
//! build subprocess, and any timestamp that lands in a release-pipeline
//! artifact (filename, embedded date field, RPM changelog header, ...)
//! must honor that env var instead of reading wall-clock time. Stages
//! that ignored SDE silently caused harness drift before
//! `crates/core/src/context::populate_time_vars` was made SDE-aware
//! (commit 5104477) — this module is the canonical helper for the
//! remaining writers that don't have a `Context` handy or that need a
//! direct `chrono::DateTime` rather than a template-var string.
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

use chrono::{DateTime, Utc};

/// Resolve "now" honoring `SOURCE_DATE_EPOCH`.
///
/// Returns the SDE-derived `DateTime<Utc>` when the env var is set to a
/// valid `i64`-parseable seconds-since-epoch value, otherwise
/// `chrono::Utc::now()`. Mirrors the resolution path in
/// `Context::populate_time_vars` so call sites can't drift out of sync
/// with the harness's SDE contract.
pub fn resolve_now() -> DateTime<Utc> {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .unwrap_or_else(Utc::now)
}

/// Return the `SOURCE_DATE_EPOCH`-derived `DateTime<Utc>` ONLY when the env
/// var is set to a valid seconds-since-epoch value, else `None`.
///
/// Distinct from [`resolve_now`], which always returns a value (falling back
/// to `Utc::now`). Use this when a call site needs to behave conditionally on
/// whether reproducibility is in effect — e.g. `stage-makeself` only injects
/// `--packaging-date` under SDE so non-harness production runs keep
/// makeself's default `LC_ALL=C date` behavior.
pub fn source_date_epoch() -> Option<DateTime<Utc>> {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env mutation is serialized via `serial_test::serial(env)` —
    /// the same group `populate_time_vars`'s tests and the template-
    /// engine tests use, so reads/writes of `SOURCE_DATE_EPOCH` never
    /// race within a single test binary.
    #[test]
    #[serial_test::serial(env)]
    fn resolve_now_honors_source_date_epoch() {
        // SAFETY: serialized via the env_source_date_epoch group.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1715000000") };
        let now = resolve_now();
        assert_eq!(now.timestamp(), 1715000000);
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    #[serial_test::serial(env)]
    fn resolve_now_ignores_malformed_sde() {
        // SAFETY: serialized via the env_source_date_epoch group.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "not-a-number") };
        // Should fall back to Utc::now(); we can't assert a specific
        // value, but the call must succeed and return a non-epoch value
        // (Utc::now() is post-2020).
        let now = resolve_now();
        assert!(now.timestamp() > 1_577_836_800); // > 2020-01-01
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    #[serial_test::serial(env)]
    fn resolve_now_falls_back_when_unset() {
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        let now = resolve_now();
        // Just check it succeeds and returns a reasonable wall-clock
        // value (post-2020). The point is the SDE branch didn't fire.
        assert!(now.timestamp() > 1_577_836_800);
    }

    #[test]
    #[serial_test::serial(env)]
    fn source_date_epoch_returns_some_when_set() {
        // SAFETY: serialized via the env_source_date_epoch group.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1715000000") };
        let dt = source_date_epoch();
        assert_eq!(dt.map(|d| d.timestamp()), Some(1715000000));
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    #[serial_test::serial(env)]
    fn source_date_epoch_returns_none_when_unset() {
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        assert!(source_date_epoch().is_none());
    }

    #[test]
    #[serial_test::serial(env)]
    fn source_date_epoch_returns_none_when_malformed() {
        // SAFETY: serialized via the env_source_date_epoch group.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "not-a-number") };
        assert!(source_date_epoch().is_none());
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }
}
