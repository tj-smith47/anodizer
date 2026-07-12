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

/// Format a seconds-since-epoch value as an RFC 3339 / ISO-8601 UTC string
/// (e.g. `2024-05-06T15:33:20+00:00`), or `None` when `secs` is out of the
/// representable range.
///
/// Used by writers that stamp a reproducible build date into an artifact
/// (e.g. the OCI `org.opencontainers.image.created` label) directly from the
/// resolved `SOURCE_DATE_EPOCH`, never from wall-clock time.
pub fn rfc3339_utc_from_epoch(secs: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

/// Format a seconds-since-epoch value as an RFC 2822 UTC string with a
/// zero-padded day (e.g. `Tue, 14 Nov 2023 22:13:20 +0000`), or `None` when
/// `secs` is out of the representable range.
///
/// Used by writers that must stamp an RFC 2822 date into an artifact header
/// from the resolved `SOURCE_DATE_EPOCH` rather than wall-clock time — notably
/// the `.zsync` `MTime:` header the AppImage stage rewrites, since `zsyncmake`
/// otherwise records the AppImage's filesystem mtime (wall-clock) and breaks
/// byte-for-byte reproducibility.
///
/// Uses an explicit `%d` (zero-padded) format rather than [`DateTime::to_rfc2822`]
/// (which renders single-digit days unpadded, `1 Jul`) so the output matches
/// `zsyncmake`'s native header byte-for-byte and the sibling RFC 2822 rendering
/// in `stage-announce`'s email `Date:`.
pub fn rfc2822_utc_from_epoch(secs: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.format("%a, %d %b %Y %H:%M:%S +0000").to_string())
}

/// Convert a unix timestamp (seconds since epoch) into the calendar fields a
/// zip (MS-DOS) `DateTime` carries: `(year, month, day, hour, minute,
/// second)`.
///
/// The zip timestamp format spans `1980..=2107` at 2-second resolution, so
/// the year is CLAMPED into that window — an out-of-range epoch (a pre-1980
/// value, or a far-future one) still yields a deterministic stamp instead of
/// degrading to a wall-clock default. This is the single source of the
/// unix→zip conversion shared by every reproducible zip writer (source
/// archives, release archives, PyPI wheels), so the three former hand-rolled
/// copies cannot drift on the clamp again.
///
/// Returns `None` only when the seconds value is outside chrono's
/// representable range.
pub fn zip_datetime_fields(epoch_secs: u64) -> Option<(u16, u8, u8, u8, u8, u8)> {
    use chrono::{Datelike as _, Timelike as _};
    let dt = DateTime::<Utc>::from_timestamp(epoch_secs as i64, 0)?;
    let year = u16::try_from(dt.year()).ok()?.clamp(1980, 2107);
    Some((
        year,
        dt.month() as u8,
        dt.day() as u8,
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_source::MapEnvSource;

    #[test]
    fn zip_datetime_fields_clamps_out_of_range_years() {
        // Epoch 0 = 1970, below zip's 1980 floor → year clamps to 1980
        // (deterministic) rather than degrading to a wall-clock default.
        let (y, ..) = zip_datetime_fields(0).expect("epoch 0 fields");
        assert_eq!(y, 1980);
        // A normal commit timestamp (2023-11-14T22:13:20Z) is preserved.
        let fields = zip_datetime_fields(1_700_000_000).expect("fields");
        assert_eq!(fields, (2023, 11, 14, 22, 13, 20));
    }

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
    fn rfc2822_matches_zsyncmake_format() {
        // 1700000000 = 2023-11-14T22:13:20Z; the exact shape zsyncmake writes
        // into a `.zsync` `MTime:` header (zero-padded day, `+0000` offset).
        assert_eq!(
            rfc2822_utc_from_epoch(1_700_000_000).as_deref(),
            Some("Tue, 14 Nov 2023 22:13:20 +0000")
        );
        // Single-digit day MUST be zero-padded (`01`), matching zsyncmake's
        // native header — chrono's to_rfc2822 would render `1 Jul` here.
        // 1751355036 = Tue, 01 Jul 2025 07:30:36 UTC.
        assert_eq!(
            rfc2822_utc_from_epoch(1_751_355_036).as_deref(),
            Some("Tue, 01 Jul 2025 07:30:36 +0000")
        );
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
