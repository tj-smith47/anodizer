use regex::Regex;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;
use tera::TeraResult;

use sha1::Digest as Sha1Digest;
use sha2::Digest as Sha2Digest;
use sha3::Digest as Sha3Digest;

// --- Helper functions for template engine ---

use super::engine_adapter::{JsonRegisterExt, try_get_value};
use crate::path_util::expand_tilde;

/// Convert a JSON template `Value` to a string for comparison purposes.
/// Numbers, bools, and strings are all stringified; null → "".
/// Returns `Cow::Borrowed` for strings (avoiding a clone), `Cow::Owned` otherwise.
fn value_to_string(v: &Value) -> Cow<'_, str> {
    match v {
        Value::String(s) => Cow::Borrowed(s.as_str()),
        Value::Number(n) => Cow::Owned(n.to_string()),
        Value::Bool(b) => Cow::Owned(b.to_string()),
        Value::Null => Cow::Borrowed(""),
        // Arrays and objects: fall back to JSON representation
        other => Cow::Owned(other.to_string()),
    }
}

/// Render a single Go/C-style `printf` value in its default (`%v`) form.
///
/// Strings render verbatim, numbers/bools render via their JSON scalar form,
/// null renders empty, and arrays/objects fall back to their JSON text.
fn printf_default(v: &Value) -> String {
    value_to_string(v).into_owned()
}

/// A parsed `printf` conversion: optional flags, width, precision, and verb.
#[derive(Clone, Copy)]
struct PrintfSpec {
    minus: bool,
    plus: bool,
    space: bool,
    zero: bool,
    hash: bool,
    width: Option<usize>,
    precision: Option<usize>,
    verb: char,
}

/// Apply width padding (respecting the `-` left-align and `0` zero-pad flags)
/// to an already-formatted body. Zero-padding is skipped for left-aligned
/// output (matching C/Go) and when a sign/prefix must stay leftmost.
fn pad(spec: &PrintfSpec, body: String, numeric_sign_prefix: Option<(&str, &str)>) -> String {
    let (sign, prefix, core) = match numeric_sign_prefix {
        Some((sign, prefix)) => (sign, prefix, body.as_str()),
        None => ("", "", body.as_str()),
    };
    let assembled = format!("{}{}{}", sign, prefix, core);
    let Some(width) = spec.width else {
        return assembled;
    };
    let len = assembled.chars().count();
    if len >= width {
        return assembled;
    }
    let padding = width - len;
    if spec.minus {
        format!("{}{}", assembled, " ".repeat(padding))
    } else if spec.zero && numeric_sign_prefix.is_some() {
        // Zero-pad after the sign/prefix so `%+04d` of 7 → `+007`.
        format!("{}{}{}{}", sign, prefix, "0".repeat(padding), core)
    } else if spec.zero {
        format!("{}{}", "0".repeat(padding), assembled)
    } else {
        format!("{}{}", " ".repeat(padding), assembled)
    }
}

/// Pad an integer conversion to width, honoring Go's rule that an explicit
/// precision DISABLES the `0` (zero-pad) flag for integer verbs — width is then
/// space-padded. `%08.5d` of 7 → `   00007`, not `00000007`. Precision already
/// supplied the zero-padding via [`int_precision`]; the `0` flag would
/// double-count. Float verbs never call this — they keep honoring `0` with
/// precision (`%08.2f` of 3.14 → `00003.14`).
fn pad_int(spec: &PrintfSpec, body: String, sign: &str, prefix: &str) -> String {
    if spec.precision.is_some() && spec.zero {
        let no_zero = PrintfSpec {
            zero: false,
            ..*spec
        };
        pad(&no_zero, body, Some((sign, prefix)))
    } else {
        pad(spec, body, Some((sign, prefix)))
    }
}

/// Compute the sign string for a signed numeric conversion given the value's
/// sign and the active `+`/space flags.
fn numeric_sign(negative: bool, plus: bool, space: bool) -> &'static str {
    if negative {
        "-"
    } else if plus {
        "+"
    } else if space {
        " "
    } else {
        ""
    }
}

/// Apply integer-precision zero-padding to an unsigned digit string.
///
/// In Go, precision on `%d`/`%b`/`%o`/`%x`/`%X` sets the MINIMUM digit count,
/// zero-left-padded (distinct from width, and applied before any sign/prefix or
/// width padding). `%.5d` of 7 → `00007`. As a special case, precision 0 of the
/// value 0 prints nothing (`%.0d` of 0 → ``), so width padding then applies to
/// the empty string.
fn int_precision(digits: &str, precision: Option<usize>) -> String {
    match precision {
        Some(0) if digits == "0" => String::new(),
        Some(p) if digits.len() < p => format!("{}{}", "0".repeat(p - digits.len()), digits),
        _ => digits.to_string(),
    }
}

/// Normalize a Rust-formatted scientific string to Go's exponent style.
///
/// Rust's `{:e}` emits an unsigned exponent with no leading zeros (`1.23e4`,
/// `1e-7`); Go always writes a sign and a minimum of two exponent digits
/// (`1.23e+04`, `1e-07`, `1e+100`). When the input has no `e`/`E` (e.g. a `%g`
/// value rendered in plain-decimal form), it is returned unchanged except for
/// the requested exponent letter case.
fn go_exponent(s: &str, uppercase: bool) -> String {
    let letter = if uppercase { 'E' } else { 'e' };
    let Some(pos) = s.find(['e', 'E']) else {
        return s.to_string();
    };
    let (mantissa, exp_part) = s.split_at(pos);
    // exp_part starts with the exponent letter; skip it to read the value.
    let exp_str = &exp_part[1..];
    let (sign, digits) = match exp_str.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exp_str.strip_prefix('+').unwrap_or(exp_str)),
    };
    // Pad to a minimum of two digits, preserving 3+ digit exponents.
    let padded = if digits.len() < 2 {
        format!("{:0>2}", digits)
    } else {
        digits.to_string()
    };
    format!("{}{}{}{}", mantissa, letter, sign, padded)
}

/// Trim trailing fractional zeros (and a now-naked decimal point) from a plain
/// decimal string, matching Go `%g`'s `%f`-branch zero-trimming.
fn trim_fraction_zeros(s: &str) -> &str {
    if !s.contains('.') {
        return s;
    }
    let trimmed = s.trim_end_matches('0');
    trimmed.strip_suffix('.').unwrap_or(trimmed)
}

/// Format a non-negative magnitude with Go `%g`/`%G` semantics.
///
/// Go selects exponential form when the decimal exponent is `< -4` or `>= eprec`
/// (where `eprec` is 6 for the default/shortest precision, otherwise the
/// requested precision), and decimal form otherwise; trailing fractional zeros
/// are trimmed in both branches. The shortest mantissa comes from Rust's
/// `{:e}`, which already yields the minimal unique digit count.
fn format_g(mag: f64, precision: Option<usize>, uppercase: bool) -> String {
    // Rust's `{:e}` gives the shortest mantissa and the decimal exponent, e.g.
    // `9.9999999e7` for 99999999.0; parse the exponent to drive the branch.
    let sci = format!("{:e}", mag);
    let exp: i32 = sci
        .split(['e', 'E'])
        .nth(1)
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);
    let eprec = precision.map(|p| p as i32).unwrap_or(6).max(1);

    if exp < -4 || exp >= eprec {
        // Exponential branch. Go uses `prec-1` fractional digits for an
        // explicit precision; for shortest it uses the minimal mantissa.
        let body = match precision {
            Some(p) => format!("{:.*e}", p.saturating_sub(1), mag),
            None => sci.clone(),
        };
        let normalized = go_exponent(&body, uppercase);
        // Trim trailing zeros in the mantissa for explicit precision (Go does).
        if precision.is_some()
            && let Some(epos) = normalized.find(['e', 'E'])
        {
            let (mantissa, exp_part) = normalized.split_at(epos);
            return format!("{}{}", trim_fraction_zeros(mantissa), exp_part);
        }
        normalized
    } else {
        // Decimal branch. For shortest, render the full decimal value and trim;
        // for explicit precision, Go uses `prec - dp` fractional digits, which
        // `trim_fraction_zeros` then collapses — emulated by formatting with
        // enough fractional digits and trimming.
        let body = match precision {
            // Significant-digit precision → fractional digits = prec - (exp+1).
            Some(p) => {
                let frac = (p as i32 - (exp + 1)).max(0) as usize;
                format!("{:.*}", frac, mag)
            }
            None => format!("{}", mag),
        };
        trim_fraction_zeros(&body).to_string()
    }
}

/// Ceiling for `printf` width and precision, guarding against an attacker (or a
/// typo) requesting a huge `" ".repeat(width)` allocation from a template.
const PRINTF_FIELD_MAX: usize = 100_000;

/// Format one `printf` verb against a value, returning a `tera::Error` for any
/// verb outside the supported bounded subset.
///
/// Supported verbs: `%s %d %v %x %X %o %b %c %q %f %e %E %g %G %t %%`, with
/// flags `- + 0 (space) #`, width, and precision.
fn format_verb(spec: &PrintfSpec, value: Option<&Value>) -> Result<String, tera::Error> {
    let val = || -> Result<&Value, tera::Error> {
        value.ok_or_else(|| {
            tera::Error::message(format!("printf: missing argument for %{}", spec.verb))
        })
    };
    match spec.verb {
        's' => {
            let mut s = printf_default(val()?);
            if let Some(prec) = spec.precision {
                s = s.chars().take(prec).collect();
            }
            Ok(pad(spec, s, None))
        }
        'v' => Ok(pad(spec, printf_default(val()?), None)),
        't' => {
            let b = val()?
                .as_bool()
                .ok_or_else(|| tera::Error::message("printf: %t expects a boolean argument"))?;
            Ok(pad(spec, b.to_string(), None))
        }
        'q' => {
            let s = printf_default(val()?);
            Ok(pad(spec, format!("{:?}", s), None))
        }
        'c' => {
            let v = val()?;
            let code = v
                .as_u64()
                .ok_or_else(|| tera::Error::message("printf: %c expects a non-negative integer"))?;
            let ch = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| {
                    tera::Error::message(format!("printf: %c: {} is not a valid code point", code))
                })?;
            Ok(pad(spec, ch.to_string(), None))
        }
        'd' => {
            let n = val()?
                .as_i64()
                .ok_or_else(|| tera::Error::message("printf: %d expects an integer argument"))?;
            let sign = numeric_sign(n < 0, spec.plus, spec.space);
            Ok(pad_int(
                spec,
                int_precision(&n.unsigned_abs().to_string(), spec.precision),
                sign,
                "",
            ))
        }
        'b' | 'o' | 'x' | 'X' => {
            let n = val()?.as_i64().ok_or_else(|| {
                tera::Error::message(format!(
                    "printf: %{} expects an integer argument",
                    spec.verb
                ))
            })?;
            let mag = n.unsigned_abs();
            let digits = match spec.verb {
                'b' => format!("{:b}", mag),
                'o' => format!("{:o}", mag),
                'x' => format!("{:x}", mag),
                'X' => format!("{:X}", mag),
                _ => unreachable!(),
            };
            let body = int_precision(&digits, spec.precision);
            let sign = numeric_sign(n < 0, spec.plus, spec.space);
            let prefix = if spec.hash {
                match spec.verb {
                    'b' => "0b",
                    'o' => "0",
                    'x' => "0x",
                    'X' => "0X",
                    _ => "",
                }
            } else {
                ""
            };
            Ok(pad_int(spec, body, sign, prefix))
        }
        'f' | 'e' | 'E' | 'g' | 'G' => {
            let f = val()?.as_f64().ok_or_else(|| {
                tera::Error::message(format!("printf: %{} expects a numeric argument", spec.verb))
            })?;
            let prec = spec.precision.unwrap_or(6);
            let mag = f.abs();
            let body = match spec.verb {
                'f' => format!("{:.*}", prec, mag),
                // Rust prints `1.23e4`; Go prints `1.23e+04` (signed exponent,
                // min two digits). Reformat the exponent to match Go so pasted
                // GoReleaser templates produce byte-identical output.
                'e' | 'E' => go_exponent(&format!("{:.*e}", prec, mag), spec.verb == 'E'),
                // %g/%G pick exponential vs decimal form per Go's rule
                // (exp < -4 or >= eprec), trimming trailing zeros.
                'g' | 'G' => format_g(mag, spec.precision, spec.verb == 'G'),
                _ => unreachable!(),
            };
            let sign = numeric_sign(f.is_sign_negative() && f != 0.0, spec.plus, spec.space);
            Ok(pad(spec, body, Some((sign, ""))))
        }
        other => Err(tera::Error::message(format!(
            "printf: unsupported verb %{} (supported: s d v x X o b c q f e E g G t %%)",
            other
        ))),
    }
}

/// Render a Go/C-style `printf` format string against its argument list.
///
/// Implements a bounded verb subset (`%s %d %v %x %X %o %b %c %q %f %e %E %g
/// %G %t %%`) with the `- + 0 (space) #` flags plus width and precision. Returns a
/// `tera::Error` on an unsupported verb or a malformed conversion rather than
/// panicking or emitting silently-wrong output.
fn sprintf(format: &str, args: &[Value]) -> Result<String, tera::Error> {
    let mut out = String::new();
    let mut arg_idx = 0usize;
    let mut chars = format.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Literal `%%`.
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }

        let mut spec = PrintfSpec {
            minus: false,
            plus: false,
            space: false,
            zero: false,
            hash: false,
            width: None,
            precision: None,
            verb: ' ',
        };

        // Flags.
        while let Some(&f) = chars.peek() {
            match f {
                '-' => spec.minus = true,
                '+' => spec.plus = true,
                ' ' => spec.space = true,
                '0' => spec.zero = true,
                '#' => spec.hash = true,
                _ => break,
            }
            chars.next();
        }

        // Width.
        let mut width_digits = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                width_digits.push(d);
                chars.next();
            } else {
                break;
            }
        }
        if !width_digits.is_empty() {
            // A value that overflows usize is, a fortiori, over the ceiling.
            let w = width_digits.parse::<usize>().unwrap_or(usize::MAX);
            if w > PRINTF_FIELD_MAX {
                return Err(tera::Error::message(format!(
                    "printf width {} exceeds maximum {}",
                    width_digits, PRINTF_FIELD_MAX
                )));
            }
            spec.width = Some(w);
        }

        // Precision.
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut prec_digits = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    prec_digits.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            // Empty precision (`%.d`) means zero; overflow means over the cap.
            let p = if prec_digits.is_empty() {
                0
            } else {
                prec_digits.parse::<usize>().unwrap_or(usize::MAX)
            };
            if p > PRINTF_FIELD_MAX {
                return Err(tera::Error::message(format!(
                    "printf precision {} exceeds maximum {}",
                    prec_digits, PRINTF_FIELD_MAX
                )));
            }
            spec.precision = Some(p);
        }

        let verb = chars.next().ok_or_else(|| {
            tera::Error::message("printf: format string ends with a dangling '%'")
        })?;
        spec.verb = verb;

        let value = args.get(arg_idx);
        out.push_str(&format_verb(&spec, value)?);
        // `%%` is the only verb that consumes no argument; it returned early above.
        arg_idx += 1;
    }

    Ok(out)
}

/// Translate a Go time format layout string to a chrono strftime format string.
///
/// Go uses a reference date (Mon Jan 2 15:04:05 MST 2006) as the layout template.
/// If the format string contains `%` characters, it's already chrono format and is
/// returned as-is. Otherwise, Go reference date components are replaced with chrono
/// strftime equivalents, longest patterns first to avoid partial matches.
pub(super) fn translate_go_time_format(fmt: &str) -> Cow<'_, str> {
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

enum VersionPart {
    Major,
    Minor,
    Patch,
}

/// Parse and increment a semver version string, returning a tera-friendly
/// error when the input isn't valid semver.
///
/// Version-increment behavior, which calls
/// `semver.MustParse(v)` and surfaces a hard template error on non-semver
/// input. Previously every component was best-effort `unwrap_or(0)`, so
/// `{{ "garbage" | incpatch }}` silently returned `"0.0.1"`.
fn increment_version(v: &str, part: VersionPart) -> Result<String, tera::Error> {
    let stripped = v.strip_prefix('v').unwrap_or(v);
    let parts: Vec<&str> = stripped.splitn(3, '.').collect();
    let invalid = || {
        tera::Error::message(format!(
            "incpatch/incminor/incmajor: '{}' is not a valid semver version (expected MAJOR.MINOR.PATCH)",
            v
        ))
    };
    if parts.len() < 3 {
        return Err(invalid());
    }
    let major: u64 = parts
        .first()
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    let minor: u64 = parts
        .get(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    let patch: u64 = parts
        .get(2)
        .and_then(|s| {
            // Handle prerelease suffix: "3-rc.1" → "3"
            s.split('-').next().and_then(|n| n.parse().ok())
        })
        .ok_or_else(invalid)?;
    let prefix = if v.starts_with('v') { "v" } else { "" };
    Ok(match part {
        VersionPart::Major => format!("{}{}.0.0", prefix, major + 1),
        VersionPart::Minor => format!("{}{}.{}.0", prefix, major, minor + 1),
        VersionPart::Patch => format!("{}{}.{}.{}", prefix, major, minor, patch + 1),
    })
}

/// Escape a string for safe inclusion inside a **double-quoted Ruby string
/// literal**: replace `\` with `\\` first, then `"` with `\"`.
///
/// Backslash must be escaped before the quote so the quote's inserted escape
/// backslash is not itself doubled. Use this anywhere a user-supplied value is
/// spliced into a `"…"` Ruby literal — both the Tera [`ruby_escape`](register_ruby_escape)
/// filter and the Rust `format!`/`push_str` sites in the Homebrew
/// formula/cask generators route through it so there is a single escape
/// implementation.
pub fn ruby_escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Register the `ruby_escape` filter on a Tera instance.
///
/// The filter delegates to [`ruby_escape_str`], making user-supplied values
/// (descriptions, homepages, display names, URLs, …) safe to interpolate into
/// `desc "…"`, `homepage "…"`, `url "…"`, and similar Homebrew formula/cask
/// stanzas without producing invalid Ruby.
///
/// Shared by both [`BASE_TERA`] and `parse_static` so that the trusted
/// formula/cask templates have the filter available even though they build a
/// fresh `tera::Tera` rather than cloning `BASE_TERA`.
pub(super) fn register_ruby_escape(tera: &mut tera::Tera) {
    tera.register_json_filter(
        "ruby_escape",
        |value: &Value, _: &HashMap<String, Value>| {
            let s = try_get_value!("ruby_escape", "value", String, value);
            Ok(Value::String(ruby_escape_str(&s)))
        },
    );
}

/// Base Tera instance with custom filters pre-registered.
/// Cloned per render() call (cheap — no templates to clone).
pub(super) static BASE_TERA: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut tera = tera::Tera::default();
    register_ruby_escape(&mut tera);

    // Compatibility aliases
    tera.register_json_filter("tolower", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("tolower", "value", String, value);
        Ok(Value::String(s.to_lowercase()))
    });
    tera.register_json_filter("toupper", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("toupper", "value", String, value);
        Ok(Value::String(s.to_uppercase()))
    });

    // trimprefix(prefix="...") — strip prefix from a string
    tera.register_json_filter(
        "trimprefix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = try_get_value!("trimprefix", "value", String, value);
            let prefix = args
                .get("prefix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimprefix requires a `prefix` argument"))?;
            let result = s.strip_prefix(prefix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // trimsuffix(suffix="...") — strip suffix from a string
    tera.register_json_filter(
        "trimsuffix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = try_get_value!("trimsuffix", "value", String, value);
            let suffix = args
                .get("suffix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimsuffix requires a `suffix` argument"))?;
            let result = s.strip_suffix(suffix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // envOrDefault and isEnvSet are registered as placeholder functions here in
    // BASE_TERA so that Tera's parser recognizes them. They are overridden with
    // context-aware closures in render() before actual rendering occurs.
    // See render() for the real implementations that read from the template
    // context's Env map.
    tera.register_json_function(
        "envOrDefault",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
            Ok(Value::String(value))
        },
    );
    tera.register_json_function(
        "isEnvSet",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("isEnvSet requires `name` argument"))?;
            let is_set = std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false);
            Ok(Value::Bool(is_set))
        },
    );

    // --- Version increment functions ---

    // incpatch("1.2.3") → "1.2.4"
    tera.register_json_function(
        "incpatch",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incpatch requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Patch)?))
        },
    );

    // incminor("1.2.3") → "1.3.0"
    tera.register_json_function(
        "incminor",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incminor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Minor)?))
        },
    );

    // incmajor("1.2.3") → "2.0.0"
    tera.register_json_function(
        "incmajor",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incmajor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Major)?))
        },
    );

    // --- Hash functions (all 14 algorithms) ---

    macro_rules! register_hash_fn {
        ($tera:expr, $name:expr, $hash_fn:expr) => {
            $tera.register_json_function(
                $name,
                |args: &HashMap<String, Value>| -> TeraResult<Value> {
                    let s = args.get("s").and_then(|v| v.as_str()).ok_or_else(|| {
                        tera::Error::message(format!("{} requires `s` argument", $name))
                    })?;
                    // Read the file; error if it cannot be read (no silent fallback).
                    let bytes = std::fs::read(s).map_err(|e| {
                        tera::Error::message(format!(
                            "{}: failed to read file '{}': {}",
                            $name, s, e
                        ))
                    })?;
                    Ok(Value::String($hash_fn(&bytes)))
                },
            );
        };
    }

    register_hash_fn!(tera, "sha1", |b: &[u8]| {
        let mut h = sha1::Sha1::new();
        Sha1Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha1Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha224", |b: &[u8]| {
        let mut h = sha2::Sha224::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha256", |b: &[u8]| {
        let mut h = sha2::Sha256::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha384", |b: &[u8]| {
        let mut h = sha2::Sha384::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha512", |b: &[u8]| {
        let mut h = sha2::Sha512::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_224", |b: &[u8]| {
        let mut h = sha3::Sha3_224::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_256", |b: &[u8]| {
        let mut h = sha3::Sha3_256::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_384", |b: &[u8]| {
        let mut h = sha3::Sha3_384::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_512", |b: &[u8]| {
        let mut h = sha3::Sha3_512::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2b", |b: &[u8]| {
        let mut h = blake2::Blake2b512::new();
        blake2::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2s", |b: &[u8]| {
        let mut h = blake2::Blake2s256::new();
        blake2::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake3", |b: &[u8]| {
        crate::hashing::hex_lower(blake3::hash(b).as_bytes())
    });
    register_hash_fn!(tera, "md5", |b: &[u8]| {
        let mut h = md5::Md5::new();
        md5::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&md5::Digest::finalize(h))
    });
    register_hash_fn!(tera, "crc32", |b: &[u8]| {
        format!("{:08x}", crc32fast::hash(b))
    });

    // --- File reading functions ---

    // readFile(path="file.txt") — reads file, returns empty string on error.
    // Intentionally returns empty on all errors (not just ENOENT).
    // Whitespace is trimmed from the result.
    tera.register_json_function(
        "readFile",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("readFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(resolved.as_ref()).unwrap_or_default();
            Ok(Value::String(content.trim().to_string()))
        },
    );

    // mustReadFile(path="file.txt") — reads file, errors if file doesn't exist
    // Whitespace is trimmed from the result.
    tera.register_json_function(
        "mustReadFile",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("mustReadFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(resolved.as_ref())
                .map_err(|e| tera::Error::message(format!("mustReadFile: {}: {}", resolved, e)))?;
            Ok(Value::String(content.trim().to_string()))
        },
    );

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

    // --- Path manipulation filters ---

    // dir — returns the directory portion of a path
    tera.register_json_filter("dir", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("dir", "value", String, value);
        let p = std::path::Path::new(&s);
        Ok(Value::String(
            p.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        ))
    });

    // base — returns the filename portion of a path
    tera.register_json_filter("base", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("base", "value", String, value);
        let p = std::path::Path::new(&s);
        Ok(Value::String(
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        ))
    });

    // abs — returns absolute path (prefixes with cwd if relative)
    tera.register_json_filter("abs", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("abs", "value", String, value);
        let p = std::path::Path::new(&s);
        if p.is_absolute() {
            Ok(Value::String(s))
        } else {
            let abs = std::env::current_dir()
                .map(|cwd| cwd.join(p).to_string_lossy().to_string())
                .unwrap_or(s);
            Ok(Value::String(abs))
        }
    });

    // urlPathEscape — URL-encode a path segment
    tera.register_json_filter(
        "urlPathEscape",
        |value: &Value, _: &HashMap<String, Value>| {
            let s = try_get_value!("urlPathEscape", "value", String, value);
            // Percent-encode all non-unreserved characters per RFC 3986.
            // Path escaping encodes `/` as `%2F`.
            let encoded: String = s
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~'
                    {
                        (b as char).to_string()
                    } else {
                        format!("%{:02X}", b)
                    }
                })
                .collect();
            Ok(Value::String(encoded))
        },
    );

    // mdv2escape — escape Telegram MarkdownV2 special characters
    tera.register_json_filter("mdv2escape", |value: &Value, _: &HashMap<String, Value>| {
        let s = try_get_value!("mdv2escape", "value", String, value);
        let escaped = s
            .chars()
            .map(|c| {
                if "_*[]()~`>#+-=|{}.!".contains(c) {
                    format!("\\{}", c)
                } else {
                    c.to_string()
                }
            })
            .collect::<String>();
        Ok(Value::String(escaped))
    });

    // --- Go-style compatibility functions ---

    // contains(s="haystack", substr="needle") — check string containment
    tera.register_json_function(
        "contains",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("contains requires `s` argument"))?;
            let substr = args
                .get("substr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("contains requires `substr` argument"))?;
            Ok(Value::Bool(s.contains(substr)))
        },
    );

    // list(items=[...]) — creates a list from an items array.
    // Note: Go-style `(list "a" "b")` syntax is handled by the preprocessor
    // (Pass 2 in template_preprocess.rs), which rewrites it to `["a", "b"]`
    // before Tera sees it. This function registration exists for direct Tera
    // usage, e.g. `{{ list(items=["a", "b"]) }}`.
    tera.register_json_function(
        "list",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::message("list requires `items` argument"))?;
            Ok(Value::Array(items.clone()))
        },
    );

    // map(pairs=[k1, v1, k2, v2, ...]) — create a map from alternating key-value pairs
    // Example: {{ $m := map "a" "1" "b" "2" }}
    tera.register_json_function(
        "map",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let pairs = args
                .get("pairs")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::message("map requires `pairs` argument"))?;
            if pairs.len() % 2 != 0 {
                return Err(tera::Error::message(
                    "map requires an even number of arguments (key-value pairs)",
                ));
            }
            let mut result = serde_json::Map::new();
            for chunk in pairs.chunks(2) {
                let key = chunk[0].as_str().unwrap_or("").to_string();
                result.insert(key, chunk[1].clone());
            }
            Ok(Value::Object(result))
        },
    );

    // in(items=[...], value="x") — check if a list contains a value
    // Go-style: {{ in (list "a" "b" "c") "b" }} → true
    // Named:    {{ in(items=["a","b","c"], value="b") }} → true
    // Compares all elements as strings.
    let in_fn = |args: &HashMap<String, Value>| -> TeraResult<Value> {
        let items = args
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                tera::Error::message("in requires `items` argument (must be an array)")
            })?;
        let value = args
            .get("value")
            .ok_or_else(|| tera::Error::message("in requires `value` argument"))?;
        // Convert the search value to a string for comparison.
        let needle = value_to_string(value);
        let found = items.iter().any(|item| value_to_string(item) == needle);
        Ok(Value::Bool(found))
    };
    tera.register_json_function("in", in_fn);
    // `contains_any` alias — avoids the Tera `in` keyword clash inside
    // `{% set x = ... %}` / `{% if ... %}` bodies.
    tera.register_json_function("contains_any", in_fn);

    // reReplaceAll(pattern="...", input="...", replacement="...") — regex replace
    // Go-style: {{ reReplaceAll "(.*)" .Message "$1" }}
    // Named:    {{ reReplaceAll(pattern="(.*)", input="hello", replacement="$1") }}
    // Supports capture group references ($1, $2, etc.).
    // Returns a Tera error on invalid regex (no panic).
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_json_function(
        "reReplaceAll",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("reReplaceAll requires `pattern` argument"))?;
            let input = args
                .get("input")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("reReplaceAll requires `input` argument"))?;
            let replacement = args
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tera::Error::message("reReplaceAll requires `replacement` argument")
                })?;
            let re = Regex::new(pattern).map_err(|e| {
                tera::Error::message(format!("reReplaceAll: invalid regex '{}': {}", pattern, e))
            })?;
            Ok(Value::String(
                re.replace_all(input, replacement).to_string(),
            ))
        },
    );

    // reReplaceAll filter form: {{ Field | reReplaceAll(pattern="...", replacement="...") }}
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_json_filter(
        "reReplaceAll",
        |value: &Value, args: &HashMap<String, Value>| {
            let input = try_get_value!("reReplaceAll", "value", String, value);
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tera::Error::message("reReplaceAll filter requires `pattern` argument")
                })?;
            let replacement = args
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tera::Error::message("reReplaceAll filter requires `replacement` argument")
                })?;
            let re = Regex::new(pattern).map_err(|e| {
                tera::Error::message(format!("reReplaceAll: invalid regex '{}': {}", pattern, e))
            })?;
            Ok(Value::String(
                re.replace_all(&input, replacement).to_string(),
            ))
        },
    );

    // englishJoin(items=[...], oxford=true) — join list items with commas and "and"
    // Empty/whitespace-only items are filtered out before joining.
    tera.register_json_function(
        "englishJoin",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::message("englishJoin requires `items` argument"))?;
            let oxford = args.get("oxford").and_then(|v| v.as_bool()).unwrap_or(true);
            let strs: Vec<String> = items
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .filter(|s| !s.trim().is_empty())
                .collect();
            let result = match strs.len() {
                0 => String::new(),
                1 => strs[0].clone(),
                2 => format!("{} and {}", strs[0], strs[1]),
                _ => {
                    // Safe: match arm `_` only reachable when `strs.len() >= 3`
                    // per the preceding 0/1/2 cases; split_last is always Some.
                    let Some((last, rest)) = strs.split_last() else {
                        return Ok(Value::String(String::new()));
                    };
                    if oxford {
                        format!("{}, and {}", rest.join(", "), last)
                    } else {
                        format!("{} and {}", rest.join(", "), last)
                    }
                }
            };
            Ok(Value::String(result))
        },
    );

    // englishJoin filter: {{ list "a" "b" "c" | englishJoin }} — pipe form
    tera.register_json_filter(
        "englishJoin",
        |value: &Value, args: &HashMap<String, Value>| {
            let items = value
                .as_array()
                .ok_or_else(|| tera::Error::message("englishJoin filter expects an array"))?;
            let oxford = args.get("oxford").and_then(|v| v.as_bool()).unwrap_or(true);
            let strs: Vec<String> = items
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .filter(|s| !s.trim().is_empty())
                .collect();
            let result = match strs.len() {
                0 => String::new(),
                1 => strs[0].clone(),
                2 => format!("{} and {}", strs[0], strs[1]),
                _ => {
                    // Safe: match arm `_` only reachable when `strs.len() >= 3`
                    // per the preceding 0/1/2 cases; split_last is always Some.
                    let Some((last, rest)) = strs.split_last() else {
                        return Ok(Value::String(String::new()));
                    };
                    if oxford {
                        format!("{}, and {}", rest.join(", "), last)
                    } else {
                        format!("{} and {}", rest.join(", "), last)
                    }
                }
            };
            Ok(Value::String(result))
        },
    );

    // filter as pipe form: {{ items | filter(regexp="pattern") }}
    tera.register_json_filter("filter", |value: &Value, args: &HashMap<String, Value>| {
        let pattern = args
            .get("regexp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::message("filter requires `regexp` argument"))?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| tera::Error::message(format!("invalid regex '{}': {}", pattern, e)))?;
        let input = value.as_str().unwrap_or("");
        let result: Vec<&str> = input.lines().filter(|line| re.is_match(line)).collect();
        Ok(Value::String(result.join("\n")))
    });

    // reverseFilter as pipe form: {{ items | reverseFilter(regexp="pattern") }}
    tera.register_json_filter(
        "reverseFilter",
        |value: &Value, args: &HashMap<String, Value>| {
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("reverseFilter requires `regexp` argument"))?;
            let re = regex::Regex::new(pattern)
                .map_err(|e| tera::Error::message(format!("invalid regex '{}': {}", pattern, e)))?;
            let input = value.as_str().unwrap_or("");
            let result: Vec<&str> = input.lines().filter(|line| !re.is_match(line)).collect();
            Ok(Value::String(result.join("\n")))
        },
    );

    // filter(items=<string|array>, regexp="pattern") — keep elements matching regex
    // Accepts a multiline STRING (splits by newline, filters lines, rejoins).
    // We also accept an array for convenience.
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_json_function(
        "filter",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let items_val = args
                .get("items")
                .ok_or_else(|| tera::Error::message("filter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("filter requires `regexp` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::message(format!("filter: invalid regex: {}", e)))?;

            if let Some(s) = items_val.as_str() {
                // String input: split by newlines, filter matching lines, rejoin
                let filtered: String = s
                    .lines()
                    .filter(|line| re.is_match(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Value::String(filtered))
            } else if let Some(arr) = items_val.as_array() {
                // Array input: filter elements whose string value matches
                let filtered: Vec<Value> = arr
                    .iter()
                    .filter(|v| v.as_str().is_some_and(|s| re.is_match(s)))
                    .cloned()
                    .collect();
                Ok(Value::Array(filtered))
            } else {
                Err(tera::Error::message(
                    "filter: `items` must be a string or array",
                ))
            }
        },
    );

    // reverseFilter(items=<string|array>, regexp="pattern") — exclude elements matching regex
    // Accepts a multiline STRING (splits by newline, filters lines, rejoins).
    // We also accept an array for convenience.
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_json_function(
        "reverseFilter",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let items_val = args
                .get("items")
                .ok_or_else(|| tera::Error::message("reverseFilter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("reverseFilter requires `regexp` argument"))?;
            let re = Regex::new(pattern).map_err(|e| {
                tera::Error::message(format!("reverseFilter: invalid regex: {}", e))
            })?;

            if let Some(s) = items_val.as_str() {
                // String input: split by newlines, exclude matching lines, rejoin
                let filtered: String = s
                    .lines()
                    .filter(|line| !re.is_match(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Value::String(filtered))
            } else if let Some(arr) = items_val.as_array() {
                // Array input: exclude elements whose string value matches
                let filtered: Vec<Value> = arr
                    .iter()
                    .filter(|v| !v.as_str().is_some_and(|s| re.is_match(s)))
                    .cloned()
                    .collect();
                Ok(Value::Array(filtered))
            } else {
                Err(tera::Error::message(
                    "reverseFilter: `items` must be a string or array",
                ))
            }
        },
    );

    // map(items={...}, key="k", default="d") — lookup a key in a map with default
    tera.register_json_function(
        "indexOrDefault",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let map = args
                .get("map")
                .and_then(|v| v.as_object())
                .ok_or_else(|| tera::Error::message("indexOrDefault requires `map` argument"))?;
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("indexOrDefault requires `key` argument"))?;
            let default = args
                .get("default")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            Ok(map.get(key).cloned().unwrap_or(default))
        },
    );

    // --- replace function ---
    // Function form: replace(s="input", old="x", new="y")
    tera.register_json_function(
        "replace",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("replace requires `s` argument"))?;
            let old = args
                .get("old")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("replace requires `old` argument"))?;
            let new = args
                .get("new")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("replace requires `new` argument"))?;
            Ok(Value::String(s.replace(old, new)))
        },
    );
    // Filter form: {{ Field | replace(from="old", to="new") }}
    // Overrides Tera's built-in replace filter. Uses `from`/`to` arg names
    // (same as the built-in) so existing Tera templates continue to work.
    tera.register_json_filter("replace", |value: &Value, args: &HashMap<String, Value>| {
        let s = try_get_value!("replace", "value", String, value);
        let from = args
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::message("replace filter requires `from` argument"))?;
        let to = args
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::message("replace filter requires `to` argument"))?;
        Ok(Value::String(s.replace(from, to)))
    });

    // --- split function ---
    // split(s="a,b,c", sep=",") → ["a", "b", "c"]
    tera.register_json_function(
        "split",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("split requires `s` argument"))?;
            let sep = args
                .get("sep")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("split requires `sep` argument"))?;
            let parts: Vec<Value> = s.split(sep).map(|p| Value::String(p.to_string())).collect();
            Ok(Value::Array(parts))
        },
    );
    // Filter form: {{ Field | split(sep=".") }}
    tera.register_json_filter("split", |value: &Value, args: &HashMap<String, Value>| {
        let s = try_get_value!("split", "value", String, value);
        let sep = args
            .get("sep")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::message("split filter requires `sep` argument"))?;
        let parts: Vec<Value> = s.split(sep).map(|p| Value::String(p.to_string())).collect();
        Ok(Value::Array(parts))
    });

    // Filter form: {{ Field | contains(substr="needle") }}
    tera.register_json_filter(
        "contains",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = try_get_value!("contains", "value", String, value);
            let substr = args.get("substr").and_then(|v| v.as_str()).ok_or_else(|| {
                tera::Error::message("contains filter requires `substr` argument")
            })?;
            Ok(Value::Bool(s.contains(substr)))
        },
    );

    // --- trim function ---
    // Function form: trim(s="  hello  ") → "hello"
    // Tera already has a built-in `trim` filter, so we only add the function form.
    tera.register_json_function(
        "trim",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trim requires `s` argument"))?;
            Ok(Value::String(s.trim().to_string()))
        },
    );

    // --- title function ---
    // Function form: title(s="hello world") → "Hello World"
    // Tera already has a built-in `title` filter, so we only add the function form.
    tera.register_json_function(
        "title",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("title requires `s` argument"))?;
            // Title-case: capitalize the first letter of each word.
            let titled = s
                .split_whitespace()
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => {
                            let upper: String = c.to_uppercase().collect();
                            format!("{}{}", upper, chars.as_str())
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(Value::String(titled))
        },
    );

    // --- Dual registration: existing filters also as functions ---

    // tolower(s="...") — function form of tolower filter
    tera.register_json_function(
        "tolower",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("tolower requires `s` argument"))?;
            Ok(Value::String(s.to_lowercase()))
        },
    );

    // toupper(s="...") — function form of toupper filter
    tera.register_json_function(
        "toupper",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("toupper requires `s` argument"))?;
            Ok(Value::String(s.to_uppercase()))
        },
    );

    // trimprefix(s="...", prefix="...") — function form of trimprefix filter
    tera.register_json_function(
        "trimprefix",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimprefix requires `s` argument"))?;
            let prefix = args
                .get("prefix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimprefix requires `prefix` argument"))?;
            let result = s.strip_prefix(prefix).unwrap_or(s);
            Ok(Value::String(result.to_string()))
        },
    );

    // trimsuffix(s="...", suffix="...") — function form of trimsuffix filter
    tera.register_json_function(
        "trimsuffix",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimsuffix requires `s` argument"))?;
            let suffix = args
                .get("suffix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("trimsuffix requires `suffix` argument"))?;
            let result = s.strip_suffix(suffix).unwrap_or(s);
            Ok(Value::String(result.to_string()))
        },
    );

    // dir(s="...") — function form of dir filter
    tera.register_json_function(
        "dir",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("dir requires `s` argument"))?;
            let p = std::path::Path::new(s);
            Ok(Value::String(
                p.parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ))
        },
    );

    // base(s="...") — function form of base filter
    tera.register_json_function(
        "base",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("base requires `s` argument"))?;
            let p = std::path::Path::new(s);
            Ok(Value::String(
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ))
        },
    );

    // abs(s="...") — function form of abs filter
    tera.register_json_function(
        "abs",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("abs requires `s` argument"))?;
            let p = std::path::Path::new(s);
            if p.is_absolute() {
                Ok(Value::String(s.to_string()))
            } else {
                let abs = std::env::current_dir()
                    .map(|cwd| cwd.join(p).to_string_lossy().to_string())
                    .unwrap_or_else(|_| s.to_string());
                Ok(Value::String(abs))
            }
        },
    );

    // urlPathEscape(s="...") — function form of urlPathEscape filter
    tera.register_json_function(
        "urlPathEscape",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("urlPathEscape requires `s` argument"))?;
            let encoded: String = s
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~'
                    {
                        (b as char).to_string()
                    } else {
                        format!("%{:02X}", b)
                    }
                })
                .collect();
            Ok(Value::String(encoded))
        },
    );

    // mdv2escape(s="...") — function form of mdv2escape filter
    tera.register_json_function(
        "mdv2escape",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("mdv2escape requires `s` argument"))?;
            let escaped = s
                .chars()
                .map(|c| {
                    if "_*[]()~`>#+-=|{}.!".contains(c) {
                        format!("\\{}", c)
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>();
            Ok(Value::String(escaped))
        },
    );

    // --- Dual registration: existing functions also as filters ---

    // incpatch — filter form: {{ "1.2.3" | incpatch }}
    tera.register_json_filter("incpatch", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incpatch", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Patch)?))
    });

    // incminor — filter form: {{ "1.2.3" | incminor }}
    tera.register_json_filter("incminor", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incminor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Minor)?))
    });

    // incmajor — filter form: {{ "1.2.3" | incmajor }}
    tera.register_json_filter("incmajor", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incmajor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Major)?))
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

    // index(map={...}, key="k") — access a map by key or array by index.
    // Go template: {{ index .Map "key" }} → access map by key.
    // Go template: {{ index .Slice 0 }} → access array by index.
    // Returns empty string if key/index not found.
    tera.register_json_function(
        "index",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let collection = args
                .get("collection")
                .ok_or_else(|| tera::Error::message("index requires `collection` argument"))?;
            let key = args
                .get("key")
                .ok_or_else(|| tera::Error::message("index requires `key` argument"))?;

            match collection {
                Value::Object(map) => {
                    let key_str = value_to_string(key);
                    Ok(map
                        .get(key_str.as_ref())
                        .cloned()
                        .unwrap_or(Value::String(String::new())))
                }
                Value::Array(arr) => {
                    if let Some(idx) = key.as_u64() {
                        Ok(arr
                            .get(idx as usize)
                            .cloned()
                            .unwrap_or(Value::String(String::new())))
                    } else {
                        Err(tera::Error::message("index: array index must be a number"))
                    }
                }
                _ => {
                    // For non-collection types, return empty string (graceful)
                    Ok(Value::String(String::new()))
                }
            }
        },
    );

    // in — filter form: {{ myList | in(value="x") }}
    // Checks whether the piped array contains the given value (string comparison).
    let in_filter = |value: &Value, args: &HashMap<String, Value>| {
        let items = value
            .as_array()
            .ok_or_else(|| tera::Error::message("in filter requires an array as input"))?;
        let needle = args
            .get("value")
            .ok_or_else(|| tera::Error::message("in filter requires `value` argument"))?;
        let needle_str = value_to_string(needle);
        let found = items.iter().any(|item| value_to_string(item) == needle_str);
        Ok(Value::Bool(found))
    };
    tera.register_json_filter("in", in_filter);
    tera.register_json_filter("contains_any", in_filter);

    // --- Go `slice` builtin (superset of Tera's native slice) ---
    // slice(start=, end=) — substring of a string (char-boundary safe) or
    // sub-slice of an array, end-exclusive (`slice(s, 0, 7)` → first 7 chars).
    // `start` is OPTIONAL (default 0) and NEGATIVE indices count from the end
    // (`start=-2` → last 2), matching Tera's native array slice so user
    // templates relying on it keep working. Go's positional `slice X 0 7` only
    // ever passes non-negative bounds, so the Go usage is a strict subset.
    tera.register_json_filter("slice", |value: &Value, args: &HashMap<String, Value>| {
        let start = args.get("start").and_then(|v| v.as_i64()).unwrap_or(0);
        let end = args.get("end").and_then(|v| v.as_i64());

        // Resolve a possibly-negative index against `len`, clamping into range.
        let resolve = |idx: i64, len: i64| -> i64 {
            let abs = if idx < 0 { len + idx } else { idx };
            abs.clamp(0, len)
        };

        match value {
            Value::String(s) => {
                let chars: Vec<char> = s.chars().collect();
                let len = chars.len() as i64;
                let lo = resolve(start, len);
                let hi = resolve(end.unwrap_or(len), len).max(lo) as usize;
                Ok(Value::String(chars[lo as usize..hi].iter().collect()))
            }
            Value::Array(arr) => {
                let len = arr.len() as i64;
                let lo = resolve(start, len);
                let hi = resolve(end.unwrap_or(len), len).max(lo) as usize;
                Ok(Value::Array(arr[lo as usize..hi].to_vec()))
            }
            other => Err(tera::Error::message(format!(
                "slice: expected a string or array, got {}",
                other
            ))),
        }
    });

    // --- Go `printf` builtin ---
    // printf(format="%04d", args=[Patch]) — formats args per a bounded Go/C
    // verb subset. Unsupported verbs return a clear error (never silent-wrong).
    tera.register_json_function(
        "printf",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let format = args
                .get("format")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("printf requires a `format` argument"))?;
            let fmt_args: Vec<Value> = args
                .get("args")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            Ok(Value::String(sprintf(format, &fmt_args)?))
        },
    );

    // --- Go `print` / `println` builtins ---
    // print(args=[a, b]) follows Go `Sprint`: a space is added between two
    // adjacent operands only when NEITHER is a string (`print 1 2` → "1 2";
    // `print "a" "b"` → "ab"; `print "a" 1` → "a1").
    // println(args=[a, b]) joins with single spaces and appends a newline.
    tera.register_json_function(
        "print",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let fmt_args: Vec<Value> = args
                .get("args")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut out = String::new();
            for (i, v) in fmt_args.iter().enumerate() {
                if i > 0 {
                    let prev_str = fmt_args[i - 1].is_string();
                    let cur_str = v.is_string();
                    if !prev_str && !cur_str {
                        out.push(' ');
                    }
                }
                out.push_str(&printf_default(v));
            }
            Ok(Value::String(out))
        },
    );
    tera.register_json_function(
        "println",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let fmt_args: Vec<Value> = args
                .get("args")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut joined = fmt_args
                .iter()
                .map(printf_default)
                .collect::<Vec<_>>()
                .join(" ");
            joined.push('\n');
            Ok(Value::String(joined))
        },
    );

    tera
});
