//! Time and date builtins: the Go-layout-to-chrono-strftime translation, the
//! `time` function, the tera 1.x-compatible `date` filter, and the `now_format`
//! filter.

use std::borrow::Cow;

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::{JsonRegisterExt, try_get_value};

/// Translate a Go time format layout string to a chrono strftime format string.
///
/// Go uses a reference date (Mon Jan 2 15:04:05 MST 2006) as the layout template.
/// If the format string contains `%` characters, it's already chrono format and is
/// returned as-is. Otherwise, Go reference date components are replaced with chrono
/// strftime equivalents, longest patterns first to avoid partial matches.
pub(crate) fn translate_go_time_format(fmt: &str) -> Cow<'_, str> {
    // If the format contains `%`, it's already chrono strftime format.
    if fmt.contains('%') {
        return Cow::Borrowed(fmt);
    }

    // Check if any Go reference date patterns are present.
    // Go reference date: Mon Jan 2 15:04:05 MST 2006
    const GO_MARKERS: &[&str] = &[
        "2006", "06", "January", "Jan", "01", "Monday", "Mon", "02", "15", "03", "04", "05", "PM",
        "pm", "-0700", "Z0700", "MST",
    ];
    let has_go_patterns = GO_MARKERS.iter().any(|p| fmt.contains(p));
    if !has_go_patterns {
        return Cow::Borrowed(fmt);
    }

    // Replace Go patterns with chrono equivalents, longest first.
    // Order matters: longer patterns must be replaced before shorter ones to avoid
    // partial matches (e.g. "January" before "Jan", "2006" before "06").
    let mut result = fmt.to_string();

    let replacements: &[(&str, &str)] = &[
        // Multi-char patterns (longest first)
        ("January", "%B"), // full month name
        ("Monday", "%A"),  // full weekday name
        ("-0700", "%z"),   // timezone offset
        ("Z0700", "%z"),   // timezone offset (Z variant)
        ("2006", "%Y"),    // 4-digit year
        ("Jan", "%b"),     // abbreviated month
        ("Mon", "%a"),     // abbreviated weekday
        ("MST", "%Z"),     // timezone name
        ("PM", "%p"),      // AM/PM
        ("pm", "%P"),      // am/pm
        ("15", "%H"),      // 24-hour
        ("06", "%y"),      // 2-digit year
        ("05", "%S"),      // second
        ("04", "%M"),      // minute
        ("03", "%I"),      // 12-hour zero-padded
        ("02", "%d"),      // zero-padded day
        ("01", "%m"),      // zero-padded month
    ];

    for (go_pat, chrono_pat) in replacements {
        result = result.replace(go_pat, chrono_pat);
    }

    Cow::Owned(result)
}

pub(super) fn register(tera: &mut tera::Tera) {
    // --- time function ---
    // time(format="%Y-%m-%d") — current UTC time formatted
    // Also accepts Go time format layout (e.g. "2006-01-02") and translates
    // to chrono strftime before formatting.
    //
    // SDE-aware: honors `SOURCE_DATE_EPOCH` so user templates that embed
    // `{{ time(format="2006-01-02") }}` in artifact names or metadata
    // produce byte-stable output under the determinism harness.
    tera.register_json_function(
        "time",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("%Y-%m-%dT%H:%M:%SZ");
            let chrono_fmt = translate_go_time_format(fmt);
            let now = crate::sde::resolve_now();
            Ok(Value::String(now.format(&chrono_fmt).to_string()))
        },
    );

    // --- date filter (tera 1.x compat) ---
    // tera 2.0 removed the builtin `date` filter; 1.x-era anodizer templates
    // (`{{ Now | date(format='%Y%m%d') }}`) are a consumer contract, so it is
    // re-registered: input is an i64 unix timestamp or a datetime string
    // (RFC3339, naive `%Y-%m-%dT%H:%M:%S`, or plain `%Y-%m-%d`); `format` is
    // chrono strftime, defaulting to `%Y-%m-%d`.
    // The tz-CONVERSION rules replicate 1.x exactly: `timezone` takes an IANA
    // name via chrono-tz (a tera 1.x DEFAULT feature, so it worked in every
    // shipped 1.x binary) and converts only timestamps and offset-carrying
    // RFC3339 strings; naive datetime and plain-date strings format as UTC
    // with `timezone` unused. One deliberate widening beyond 1.x: the
    // no-timezone timestamp path formats a `DateTime<Utc>` (so `%Z` renders
    // `UTC` and `%z` renders `+0000`) where 1.x formatted a bare
    // `NaiveDateTime`, whose `%Z`/`%z` formatting failed at render — a
    // panic→defined-output widening, never a changed byte for any format
    // 1.x could actually render.
    // 1.x's `locale` argument needed tera's non-default `date-locale`
    // feature; without it 1.x never read the argument and SILENTLY formatted
    // in the POSIX locale. anodizer chooses to error instead — silently
    // dropping an explicit argument hides the misconfiguration.
    tera.register_json_filter("date", |value: &Value, args: &HashMap<String, Value>| {
        use chrono::format::{Item, StrftimeItems};
        use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
        use chrono_tz::Tz;

        if args.contains_key("locale") {
            return Err(tera::Error::message(
                "date: the `locale` argument is not supported; remove it — \
                 output is always formatted in the POSIX locale",
            ));
        }
        let format = args
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("%Y-%m-%d");
        if StrftimeItems::new(format).any(|item| matches!(item, Item::Error)) {
            return Err(tera::Error::message(format!(
                "date: invalid format `{format}` — expected chrono strftime \
                 specifiers (e.g. `%Y-%m-%d`)"
            )));
        }
        let timezone = match args.get("timezone") {
            Some(val) => {
                let tz = try_get_value!("date", "timezone", String, val);
                Some(tz.parse::<Tz>().map_err(|_| {
                    tera::Error::message(format!(
                        "date: error parsing `{tz}` as a timezone \
                         (expected an IANA name, e.g. `America/New_York`)"
                    ))
                })?)
            }
            None => None,
        };

        let formatted = match value {
            Value::Number(n) => {
                let secs = n.as_i64().ok_or_else(|| {
                    tera::Error::message(format!("date: expected an integer timestamp, got {n}"))
                })?;
                let dt = DateTime::<Utc>::from_timestamp(secs, 0).ok_or_else(|| {
                    tera::Error::message(format!("date: timestamp {secs} is out of range"))
                })?;
                match timezone {
                    Some(tz) => tz
                        .from_utc_datetime(&dt.naive_utc())
                        .format(format)
                        .to_string(),
                    None => dt.format(format).to_string(),
                }
            }
            Value::String(s) if s.contains('T') => match s.parse::<DateTime<FixedOffset>>() {
                Ok(dt) => match timezone {
                    Some(tz) => dt.with_timezone(&tz).format(format).to_string(),
                    None => dt.format(format).to_string(),
                },
                Err(_) => {
                    let dt = s.parse::<NaiveDateTime>().map_err(|_| {
                        tera::Error::message(format!(
                            "date: cannot parse `{s}` as an RFC3339 or naive datetime"
                        ))
                    })?;
                    // tera 1.x treated an offset-less datetime as UTC and did
                    // NOT apply `timezone` to it; replicated for parity.
                    dt.and_utc().format(format).to_string()
                }
            },
            Value::String(s) => {
                let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
                    tera::Error::message(format!("date: cannot parse `{s}` as a YYYY-MM-DD date"))
                })?;
                // Midnight always exists for a valid NaiveDate; unwrap_or is
                // unreachable but keeps the closure panic-free.
                d.and_hms_opt(0, 0, 0)
                    .unwrap_or_default()
                    .and_utc()
                    .format(format)
                    .to_string()
            }
            other => {
                return Err(tera::Error::message(format!(
                    "date: expected a timestamp or datetime string, got {other}"
                )));
            }
        };
        Ok(Value::String(formatted))
    });

    // now_format — filter form: {{ Now | now_format(format="2006-01-02") }}
    // Formats the current UTC time using the given format string.
    // Accepts both Go time layout (e.g. "2006-01-02") and chrono strftime
    // (e.g. "%Y-%m-%d"). The piped value (Now) is ignored — the filter always
    // uses the current UTC time, the `.Now.Format` behavior.
    //
    // SDE-aware: honors `SOURCE_DATE_EPOCH` so the harness's two from-clean
    // rebuilds produce identical output for templates like
    // `{{ Now | now_format(format="2006-01-02") }}`.
    tera.register_json_filter(
        "now_format",
        |_value: &Value, args: &HashMap<String, Value>| {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("now_format requires a `format` argument"))?;
            let chrono_fmt = translate_go_time_format(fmt);
            let now = crate::sde::resolve_now();
            Ok(Value::String(now.format(&chrono_fmt).to_string()))
        },
    );
}
