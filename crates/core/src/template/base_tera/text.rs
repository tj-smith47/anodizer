//! String builtins: case conversion, prefix/suffix trimming, substring search,
//! literal and regex replacement, splitting, title-casing, and the Ruby and
//! Telegram MarkdownV2 literal escapers.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::{JsonRegisterExt, try_get_value};

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
pub(crate) fn register_ruby_escape(tera: &mut tera::Tera) {
    tera.register_json_filter(
        "ruby_escape",
        |value: &Value, _: &HashMap<String, Value>| {
            let s = try_get_value!("ruby_escape", "value", String, value);
            Ok(Value::String(ruby_escape_str(&s)))
        },
    );
}

pub(super) fn register(tera: &mut tera::Tera) {
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
}
