//! The Go `printf` / `print` / `println` builtins and the `Sprintf` engine
//! behind them: conversion-spec parsing, flag/width/precision padding, and the
//! per-verb formatters for the supported `%` verb subset.

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::JsonRegisterExt;

use super::value_to_string;

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

pub(super) fn register(tera: &mut tera::Tera) {
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
}
