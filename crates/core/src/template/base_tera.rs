use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;
use tera::Value;

use sha1::Digest as Sha1Digest;
use sha2::Digest as Sha2Digest;
use sha3::Digest as Sha3Digest;

// --- Helper functions for template engine ---

/// Expand a leading `~/` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}/{}", home, rest);
    }
    path.to_string()
}

/// Convert a Tera `Value` to a string for comparison purposes.
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
        tera::Error::msg(format!(
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

/// Base Tera instance with custom filters pre-registered.
/// Cloned per render() call (cheap — no templates to clone).
pub(super) static BASE_TERA: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut tera = tera::Tera::default();

    // Compatibility aliases
    tera.register_filter("tolower", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("tolower", "value", String, value);
        Ok(Value::String(s.to_lowercase()))
    });
    tera.register_filter("toupper", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("toupper", "value", String, value);
        Ok(Value::String(s.to_uppercase()))
    });

    // trimprefix(prefix="...") — strip prefix from a string
    tera.register_filter(
        "trimprefix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = tera::try_get_value!("trimprefix", "value", String, value);
            let prefix = args
                .get("prefix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimprefix requires a `prefix` argument"))?;
            let result = s.strip_prefix(prefix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // trimsuffix(suffix="...") — strip suffix from a string
    tera.register_filter(
        "trimsuffix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = tera::try_get_value!("trimsuffix", "value", String, value);
            let suffix = args
                .get("suffix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimsuffix requires a `suffix` argument"))?;
            let result = s.strip_suffix(suffix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // envOrDefault and isEnvSet are registered as placeholder functions here in
    // BASE_TERA so that Tera's parser recognizes them. They are overridden with
    // context-aware closures in render() before actual rendering occurs.
    // See render() for the real implementations that read from the template
    // context's Env map.
    tera.register_function(
        "envOrDefault",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
            Ok(Value::String(value))
        },
    );
    tera.register_function(
        "isEnvSet",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("isEnvSet requires `name` argument"))?;
            let is_set = std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false);
            Ok(Value::Bool(is_set))
        },
    );

    // --- Version increment functions ---

    // incpatch("1.2.3") → "1.2.4"
    tera.register_function(
        "incpatch",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("incpatch requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Patch)?))
        },
    );

    // incminor("1.2.3") → "1.3.0"
    tera.register_function(
        "incminor",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("incminor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Minor)?))
        },
    );

    // incmajor("1.2.3") → "2.0.0"
    tera.register_function(
        "incmajor",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("incmajor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Major)?))
        },
    );

    // --- Hash functions (all 14 algorithms) ---

    macro_rules! register_hash_fn {
        ($tera:expr, $name:expr, $hash_fn:expr) => {
            $tera.register_function(
                $name,
                |args: &HashMap<String, Value>| -> tera::Result<Value> {
                    let s = args.get("s").and_then(|v| v.as_str()).ok_or_else(|| {
                        tera::Error::msg(format!("{} requires `s` argument", $name))
                    })?;
                    // Read the file; error if it cannot be read (no silent fallback).
                    let bytes = std::fs::read(s).map_err(|e| {
                        tera::Error::msg(format!("{}: failed to read file '{}': {}", $name, s, e))
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
    tera.register_function(
        "readFile",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("readFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(resolved).unwrap_or_default();
            Ok(Value::String(content.trim().to_string()))
        },
    );

    // mustReadFile(path="file.txt") — reads file, errors if file doesn't exist
    // Whitespace is trimmed from the result.
    tera.register_function(
        "mustReadFile",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("mustReadFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(&resolved)
                .map_err(|e| tera::Error::msg(format!("mustReadFile: {}: {}", resolved, e)))?;
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
    tera.register_function(
        "time",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
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
    tera.register_filter("dir", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("dir", "value", String, value);
        let p = std::path::Path::new(&s);
        Ok(Value::String(
            p.parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        ))
    });

    // base — returns the filename portion of a path
    tera.register_filter("base", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("base", "value", String, value);
        let p = std::path::Path::new(&s);
        Ok(Value::String(
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        ))
    });

    // abs — returns absolute path (prefixes with cwd if relative)
    tera.register_filter("abs", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("abs", "value", String, value);
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
    tera.register_filter(
        "urlPathEscape",
        |value: &Value, _: &HashMap<String, Value>| {
            let s = tera::try_get_value!("urlPathEscape", "value", String, value);
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
    tera.register_filter("mdv2escape", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("mdv2escape", "value", String, value);
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
    tera.register_function(
        "contains",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("contains requires `s` argument"))?;
            let substr = args
                .get("substr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("contains requires `substr` argument"))?;
            Ok(Value::Bool(s.contains(substr)))
        },
    );

    // list(items=[...]) — creates a list from an items array.
    // Note: Go-style `(list "a" "b")` syntax is handled by the preprocessor
    // (Pass 2 in template_preprocess.rs), which rewrites it to `["a", "b"]`
    // before Tera sees it. This function registration exists for direct Tera
    // usage, e.g. `{{ list(items=["a", "b"]) }}`.
    tera.register_function(
        "list",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::msg("list requires `items` argument"))?;
            Ok(Value::Array(items.clone()))
        },
    );

    // map(pairs=[k1, v1, k2, v2, ...]) — create a map from alternating key-value pairs
    // Example: {{ $m := map "a" "1" "b" "2" }}
    tera.register_function(
        "map",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let pairs = args
                .get("pairs")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::msg("map requires `pairs` argument"))?;
            if pairs.len() % 2 != 0 {
                return Err(tera::Error::msg(
                    "map requires an even number of arguments (key-value pairs)",
                ));
            }
            let mut result = tera::Map::new();
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
    let in_fn = |args: &HashMap<String, Value>| -> tera::Result<Value> {
        let items = args
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| tera::Error::msg("in requires `items` argument (must be an array)"))?;
        let value = args
            .get("value")
            .ok_or_else(|| tera::Error::msg("in requires `value` argument"))?;
        // Convert the search value to a string for comparison.
        let needle = value_to_string(value);
        let found = items.iter().any(|item| value_to_string(item) == needle);
        Ok(Value::Bool(found))
    };
    tera.register_function("in", in_fn);
    // `contains_any` alias — avoids the Tera `in` keyword clash inside
    // `{% set x = ... %}` / `{% if ... %}` bodies.
    tera.register_function("contains_any", in_fn);

    // reReplaceAll(pattern="...", input="...", replacement="...") — regex replace
    // Go-style: {{ reReplaceAll "(.*)" .Message "$1" }}
    // Named:    {{ reReplaceAll(pattern="(.*)", input="hello", replacement="$1") }}
    // Supports capture group references ($1, $2, etc.).
    // Returns a Tera error on invalid regex (no panic).
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_function(
        "reReplaceAll",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reReplaceAll requires `pattern` argument"))?;
            let input = args
                .get("input")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reReplaceAll requires `input` argument"))?;
            let replacement = args
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reReplaceAll requires `replacement` argument"))?;
            let re = Regex::new(pattern).map_err(|e| {
                tera::Error::msg(format!("reReplaceAll: invalid regex '{}': {}", pattern, e))
            })?;
            Ok(Value::String(
                re.replace_all(input, replacement).to_string(),
            ))
        },
    );

    // reReplaceAll filter form: {{ Field | reReplaceAll(pattern="...", replacement="...") }}
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_filter(
        "reReplaceAll",
        |value: &Value, args: &HashMap<String, Value>| {
            let input = tera::try_get_value!("reReplaceAll", "value", String, value);
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tera::Error::msg("reReplaceAll filter requires `pattern` argument")
                })?;
            let replacement = args
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    tera::Error::msg("reReplaceAll filter requires `replacement` argument")
                })?;
            let re = Regex::new(pattern).map_err(|e| {
                tera::Error::msg(format!("reReplaceAll: invalid regex '{}': {}", pattern, e))
            })?;
            Ok(Value::String(
                re.replace_all(&input, replacement).to_string(),
            ))
        },
    );

    // englishJoin(items=[...], oxford=true) — join list items with commas and "and"
    // Empty/whitespace-only items are filtered out before joining.
    tera.register_function(
        "englishJoin",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::msg("englishJoin requires `items` argument"))?;
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
    tera.register_filter(
        "englishJoin",
        |value: &Value, args: &HashMap<String, Value>| {
            let items = value
                .as_array()
                .ok_or_else(|| tera::Error::msg("englishJoin filter expects an array"))?;
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
    tera.register_filter("filter", |value: &Value, args: &HashMap<String, Value>| {
        let pattern = args
            .get("regexp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::msg("filter requires `regexp` argument"))?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| tera::Error::msg(format!("invalid regex '{}': {}", pattern, e)))?;
        let input = value.as_str().unwrap_or("");
        let result: Vec<&str> = input.lines().filter(|line| re.is_match(line)).collect();
        Ok(Value::String(result.join("\n")))
    });

    // reverseFilter as pipe form: {{ items | reverseFilter(regexp="pattern") }}
    tera.register_filter(
        "reverseFilter",
        |value: &Value, args: &HashMap<String, Value>| {
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reverseFilter requires `regexp` argument"))?;
            let re = regex::Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("invalid regex '{}': {}", pattern, e)))?;
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
    tera.register_function(
        "filter",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items_val = args
                .get("items")
                .ok_or_else(|| tera::Error::msg("filter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("filter requires `regexp` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("filter: invalid regex: {}", e)))?;

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
                Err(tera::Error::msg(
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
    tera.register_function(
        "reverseFilter",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items_val = args
                .get("items")
                .ok_or_else(|| tera::Error::msg("reverseFilter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reverseFilter requires `regexp` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("reverseFilter: invalid regex: {}", e)))?;

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
                Err(tera::Error::msg(
                    "reverseFilter: `items` must be a string or array",
                ))
            }
        },
    );

    // map(items={...}, key="k", default="d") — lookup a key in a map with default
    tera.register_function(
        "indexOrDefault",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let map = args
                .get("map")
                .and_then(|v| v.as_object())
                .ok_or_else(|| tera::Error::msg("indexOrDefault requires `map` argument"))?;
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("indexOrDefault requires `key` argument"))?;
            let default = args
                .get("default")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            Ok(map.get(key).cloned().unwrap_or(default))
        },
    );

    // --- replace function ---
    // Function form: replace(s="input", old="x", new="y")
    tera.register_function(
        "replace",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("replace requires `s` argument"))?;
            let old = args
                .get("old")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("replace requires `old` argument"))?;
            let new = args
                .get("new")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("replace requires `new` argument"))?;
            Ok(Value::String(s.replace(old, new)))
        },
    );
    // Filter form: {{ Field | replace(from="old", to="new") }}
    // Overrides Tera's built-in replace filter. Uses `from`/`to` arg names
    // (same as the built-in) so existing Tera templates continue to work.
    tera.register_filter("replace", |value: &Value, args: &HashMap<String, Value>| {
        let s = tera::try_get_value!("replace", "value", String, value);
        let from = args
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::msg("replace filter requires `from` argument"))?;
        let to = args
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::msg("replace filter requires `to` argument"))?;
        Ok(Value::String(s.replace(from, to)))
    });

    // --- split function ---
    // split(s="a,b,c", sep=",") → ["a", "b", "c"]
    tera.register_function(
        "split",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("split requires `s` argument"))?;
            let sep = args
                .get("sep")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("split requires `sep` argument"))?;
            let parts: Vec<Value> = s.split(sep).map(|p| Value::String(p.to_string())).collect();
            Ok(Value::Array(parts))
        },
    );
    // Filter form: {{ Field | split(sep=".") }}
    tera.register_filter("split", |value: &Value, args: &HashMap<String, Value>| {
        let s = tera::try_get_value!("split", "value", String, value);
        let sep = args
            .get("sep")
            .and_then(|v| v.as_str())
            .ok_or_else(|| tera::Error::msg("split filter requires `sep` argument"))?;
        let parts: Vec<Value> = s.split(sep).map(|p| Value::String(p.to_string())).collect();
        Ok(Value::Array(parts))
    });

    // Filter form: {{ Field | contains(substr="needle") }}
    tera.register_filter(
        "contains",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = tera::try_get_value!("contains", "value", String, value);
            let substr = args
                .get("substr")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("contains filter requires `substr` argument"))?;
            Ok(Value::Bool(s.contains(substr)))
        },
    );

    // --- trim function ---
    // Function form: trim(s="  hello  ") → "hello"
    // Tera already has a built-in `trim` filter, so we only add the function form.
    tera.register_function(
        "trim",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trim requires `s` argument"))?;
            Ok(Value::String(s.trim().to_string()))
        },
    );

    // --- title function ---
    // Function form: title(s="hello world") → "Hello World"
    // Tera already has a built-in `title` filter, so we only add the function form.
    tera.register_function(
        "title",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("title requires `s` argument"))?;
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
    tera.register_function(
        "tolower",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("tolower requires `s` argument"))?;
            Ok(Value::String(s.to_lowercase()))
        },
    );

    // toupper(s="...") — function form of toupper filter
    tera.register_function(
        "toupper",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("toupper requires `s` argument"))?;
            Ok(Value::String(s.to_uppercase()))
        },
    );

    // trimprefix(s="...", prefix="...") — function form of trimprefix filter
    tera.register_function(
        "trimprefix",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimprefix requires `s` argument"))?;
            let prefix = args
                .get("prefix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimprefix requires `prefix` argument"))?;
            let result = s.strip_prefix(prefix).unwrap_or(s);
            Ok(Value::String(result.to_string()))
        },
    );

    // trimsuffix(s="...", suffix="...") — function form of trimsuffix filter
    tera.register_function(
        "trimsuffix",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimsuffix requires `s` argument"))?;
            let suffix = args
                .get("suffix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimsuffix requires `suffix` argument"))?;
            let result = s.strip_suffix(suffix).unwrap_or(s);
            Ok(Value::String(result.to_string()))
        },
    );

    // dir(s="...") — function form of dir filter
    tera.register_function(
        "dir",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("dir requires `s` argument"))?;
            let p = std::path::Path::new(s);
            Ok(Value::String(
                p.parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ))
        },
    );

    // base(s="...") — function form of base filter
    tera.register_function(
        "base",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("base requires `s` argument"))?;
            let p = std::path::Path::new(s);
            Ok(Value::String(
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ))
        },
    );

    // abs(s="...") — function form of abs filter
    tera.register_function(
        "abs",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("abs requires `s` argument"))?;
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
    tera.register_function(
        "urlPathEscape",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("urlPathEscape requires `s` argument"))?;
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
    tera.register_function(
        "mdv2escape",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let s = args
                .get("s")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("mdv2escape requires `s` argument"))?;
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
    tera.register_filter("incpatch", |value: &Value, _: &HashMap<String, Value>| {
        let v = tera::try_get_value!("incpatch", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Patch)?))
    });

    // incminor — filter form: {{ "1.2.3" | incminor }}
    tera.register_filter("incminor", |value: &Value, _: &HashMap<String, Value>| {
        let v = tera::try_get_value!("incminor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Minor)?))
    });

    // incmajor — filter form: {{ "1.2.3" | incmajor }}
    tera.register_filter("incmajor", |value: &Value, _: &HashMap<String, Value>| {
        let v = tera::try_get_value!("incmajor", "value", String, value);
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
    tera.register_filter(
        "now_format",
        |_value: &Value, args: &HashMap<String, Value>| {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("now_format requires a `format` argument"))?;
            let chrono_fmt = translate_go_time_format(fmt);
            let now = crate::sde::resolve_now();
            Ok(Value::String(now.format(&chrono_fmt).to_string()))
        },
    );

    // index(map={...}, key="k") — access a map by key or array by index.
    // Go template: {{ index .Map "key" }} → access map by key.
    // Go template: {{ index .Slice 0 }} → access array by index.
    // Returns empty string if key/index not found.
    tera.register_function(
        "index",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let collection = args
                .get("collection")
                .ok_or_else(|| tera::Error::msg("index requires `collection` argument"))?;
            let key = args
                .get("key")
                .ok_or_else(|| tera::Error::msg("index requires `key` argument"))?;

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
                        Err(tera::Error::msg("index: array index must be a number"))
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
            .ok_or_else(|| tera::Error::msg("in filter requires an array as input"))?;
        let needle = args
            .get("value")
            .ok_or_else(|| tera::Error::msg("in filter requires `value` argument"))?;
        let needle_str = value_to_string(needle);
        let found = items.iter().any(|item| value_to_string(item) == needle_str);
        Ok(Value::Bool(found))
    };
    tera.register_filter("in", in_filter);
    tera.register_filter("contains_any", in_filter);

    tera
});
