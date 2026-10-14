//! The source date: the one reader of `SOURCE_DATE_EPOCH` and
//! `ANODIZE_SOURCE_DATE_EPOCH`, and the helpers that turn it into dates.
//!
//! [`ambient_epoch_with_env`] is the only place either variable is parsed.
//! Its rules hold for every reader in every mode:
//!
//! 1. `SOURCE_DATE_EPOCH` is read first; `ANODIZE_SOURCE_DATE_EPOCH` is read
//!    only when `SOURCE_DATE_EPOCH` is unset or empty.
//! 2. An empty or whitespace-only value counts as unset.
//! 3. Any other value must be a whole number of seconds from 0 up. A value
//!    that is not is an error that names the variable and the value.
//!
//! [`reproducible_epoch_with_env`] adds the commit-time fallback, and
//! [`Context::resolve_reproducible_mtime`](crate::context::Context::resolve_reproducible_mtime)
//! applies it to a run, so the cargo child, binary and archive mtimes,
//! wheels, the docker `created` label and SBOM timestamps carry one date.
//!
//! [`resolve_now_with_env`] and [`source_date_epoch_with_env`] read through
//! the same reader, so a malformed value is the same error in every command,
//! whether it reads the date through
//! [`Context::populate_time_vars`](crate::context::Context::populate_time_vars)
//! or directly.

use crate::env_source::EnvSource;
use chrono::{DateTime, Utc};

/// The standard variable that fixes every reproducible timestamp.
pub const SOURCE_DATE_EPOCH: &str = "SOURCE_DATE_EPOCH";

/// The older anodizer spelling of [`SOURCE_DATE_EPOCH`], read when the
/// standard variable is unset or empty.
pub const ANODIZE_SOURCE_DATE_EPOCH: &str = "ANODIZE_SOURCE_DATE_EPOCH";

/// Read the source date the environment sets, as seconds since
/// 1970-01-01 UTC.
///
/// Returns `Ok(None)` when neither variable holds a value, and an error naming
/// the variable and its value when the value is not a whole number of
/// seconds from 0 up that a date can hold. See the module docs for the order.
pub fn ambient_epoch_with_env<E: EnvSource + ?Sized>(env: &E) -> anyhow::Result<Option<i64>> {
    for name in [SOURCE_DATE_EPOCH, ANODIZE_SOURCE_DATE_EPOCH] {
        let Some(raw) = env.var(name) else {
            continue;
        };
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        return value
            .parse::<i64>()
            .ok()
            .filter(|secs| *secs >= 0 && DateTime::<Utc>::from_timestamp(*secs, 0).is_some())
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{name} is set to {raw:?}, which is not a whole number of seconds since \
                     1970-01-01 UTC; set it to a value such as 1700000000, or unset it"
                )
            });
    }
    Ok(None)
}

/// The reproducible timestamp for a run: the source date from
/// [`ambient_epoch_with_env`] when set, otherwise `commit_timestamp` (the
/// HEAD committer time in Unix seconds).
///
/// A missing, unparsable or zero commit timestamp means no commit is known,
/// so the result is `Ok(None)` when the environment sets nothing either. A
/// source date of 0 from the environment is kept, since the operator chose it.
pub fn reproducible_epoch_with_env<E: EnvSource + ?Sized>(
    env: &E,
    commit_timestamp: Option<&str>,
) -> anyhow::Result<Option<u64>> {
    if let Some(secs) = ambient_epoch_with_env(env)? {
        return Ok(u64::try_from(secs).ok());
    }
    Ok(commit_timestamp
        .and_then(|ts| ts.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0))
}

/// Resolve "now": the source date from [`ambient_epoch_with_env`] when set,
/// otherwise `chrono::Utc::now()`.
///
/// Fails with the reader's error when a source date variable is malformed.
pub fn resolve_now_with_env<E: EnvSource + ?Sized>(env: &E) -> anyhow::Result<DateTime<Utc>> {
    Ok(source_date_epoch_with_env(env)?.unwrap_or_else(Utc::now))
}

/// Return the source date from [`ambient_epoch_with_env`] as a
/// `DateTime<Utc>` when one is set, else `None`.
///
/// Use this when a call site behaves differently when reproducibility is in
/// effect: `stage-makeself` passes `--packaging-date` only under a source
/// date, so other runs keep makeself's default `LC_ALL=C date`. Fails with the
/// reader's error when a source date variable is malformed.
pub fn source_date_epoch_with_env<E: EnvSource + ?Sized>(
    env: &E,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    Ok(ambient_epoch_with_env(env)?.and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0)))
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
/// from the resolved `SOURCE_DATE_EPOCH` — notably
/// the `.zsync` `MTime:` header the AppImage stage rewrites, since `zsyncmake`
/// otherwise records the AppImage's filesystem mtime (wall-clock) and breaks
/// byte-for-byte reproducibility.
///
/// Uses an explicit `%d` (zero-padded) format; [`DateTime::to_rfc2822`]
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
/// value, or a far-future one) still yields a deterministic stamp and never
/// degrades to a wall-clock default. This is the single source of the
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
        // Epoch 0 = 1970, below zip's 1980 minimum → year clamps to 1980
        // (deterministic) and never degrades to a wall-clock default.
        let (y, ..) = zip_datetime_fields(0).expect("epoch 0 fields");
        assert_eq!(y, 1980);
        // A normal commit timestamp (2023-11-14T22:13:20Z) is preserved.
        let fields = zip_datetime_fields(1_700_000_000).expect("fields");
        assert_eq!(fields, (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn resolve_now_honors_source_date_epoch() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let now = resolve_now_with_env(&env).unwrap();
        assert_eq!(now.timestamp(), 1715000000);
    }

    #[test]
    fn resolve_now_refuses_a_malformed_source_date() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "not-a-number");
        let err = resolve_now_with_env(&env).unwrap_err().to_string();
        assert!(
            err.starts_with("SOURCE_DATE_EPOCH is set to \"not-a-number\""),
            "{err}"
        );
    }

    #[test]
    fn an_empty_source_date_epoch_counts_as_unset() {
        let env = MapEnvSource::new().with(SOURCE_DATE_EPOCH, "");
        assert_eq!(ambient_epoch_with_env(&env).unwrap(), None);
        let env = MapEnvSource::new().with(SOURCE_DATE_EPOCH, "  ");
        assert_eq!(ambient_epoch_with_env(&env).unwrap(), None);
    }

    #[test]
    fn an_empty_source_date_epoch_falls_through_to_the_older_spelling() {
        let env = MapEnvSource::new()
            .with(SOURCE_DATE_EPOCH, "")
            .with(ANODIZE_SOURCE_DATE_EPOCH, "1600000000");
        assert_eq!(ambient_epoch_with_env(&env).unwrap(), Some(1_600_000_000));
    }

    #[test]
    fn source_date_epoch_wins_over_the_older_spelling() {
        let env = MapEnvSource::new()
            .with(SOURCE_DATE_EPOCH, "1700000000")
            .with(ANODIZE_SOURCE_DATE_EPOCH, "1600000000");
        assert_eq!(ambient_epoch_with_env(&env).unwrap(), Some(1_700_000_000));
    }

    #[test]
    fn the_older_spelling_is_read_when_it_is_the_only_one_set() {
        let env = MapEnvSource::new().with(ANODIZE_SOURCE_DATE_EPOCH, "1600000000");
        assert_eq!(ambient_epoch_with_env(&env).unwrap(), Some(1_600_000_000));
    }

    #[test]
    fn a_malformed_value_is_an_error_naming_the_variable_and_the_value() {
        for (name, value) in [
            (SOURCE_DATE_EPOCH, "soon"),
            (SOURCE_DATE_EPOCH, "-5"),
            (SOURCE_DATE_EPOCH, "1.5"),
            (ANODIZE_SOURCE_DATE_EPOCH, "tomorrow"),
        ] {
            let env = MapEnvSource::new().with(name, value);
            let err = ambient_epoch_with_env(&env).unwrap_err().to_string();
            assert!(
                err.starts_with(&format!("{name} is set to \"{value}\"")),
                "{err}"
            );
        }
    }

    #[test]
    fn a_malformed_source_date_epoch_is_not_hidden_by_the_older_spelling() {
        let env = MapEnvSource::new()
            .with(SOURCE_DATE_EPOCH, "soon")
            .with(ANODIZE_SOURCE_DATE_EPOCH, "1600000000");
        assert!(ambient_epoch_with_env(&env).is_err());
    }

    #[test]
    fn the_reproducible_epoch_prefers_the_environment_over_the_commit_time() {
        let env = MapEnvSource::new().with(SOURCE_DATE_EPOCH, "1600000000");
        assert_eq!(
            reproducible_epoch_with_env(&env, Some("1700000000")).unwrap(),
            Some(1_600_000_000)
        );
        let env = MapEnvSource::new();
        assert_eq!(
            reproducible_epoch_with_env(&env, Some("1700000000")).unwrap(),
            Some(1_700_000_000)
        );
        assert_eq!(reproducible_epoch_with_env(&env, Some("0")).unwrap(), None);
        assert_eq!(reproducible_epoch_with_env(&env, Some("")).unwrap(), None);
        assert_eq!(
            reproducible_epoch_with_env(&env, Some("not-a-number")).unwrap(),
            None
        );
        assert_eq!(reproducible_epoch_with_env(&env, None).unwrap(), None);
        let env = MapEnvSource::new().with(SOURCE_DATE_EPOCH, "soon");
        assert!(reproducible_epoch_with_env(&env, Some("1700000000")).is_err());
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
        let now = resolve_now_with_env(&env).unwrap();
        assert!(now.timestamp() > 1_577_836_800);
    }

    #[test]
    fn source_date_epoch_returns_some_when_set() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let dt = source_date_epoch_with_env(&env).unwrap();
        assert_eq!(dt.map(|d| d.timestamp()), Some(1715000000));
    }

    #[test]
    fn source_date_epoch_returns_none_when_unset() {
        let env = MapEnvSource::new();
        assert!(source_date_epoch_with_env(&env).unwrap().is_none());
    }

    #[test]
    fn source_date_epoch_refuses_a_malformed_value() {
        let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "not-a-number");
        assert!(source_date_epoch_with_env(&env).is_err());
    }
}
