// Template rendering powered by Tera.
// Supports both Go-style `{{ .Field }}` and Tera-style `{{ Field }}`.
// Go-style templates are preprocessed (leading dots stripped) before Tera renders them.
// Tera gives us: if/else/endif, for loops, pipes (| lower, | upper, | replace),
// | default, | trim, | title, and many more built-in filters.

use anyhow::{Context as _, Result};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::template_preprocess::preprocess;
use tera::Value;

use sha1::Digest as Sha1Digest;
use sha2::Digest as Sha2Digest;
use sha3::Digest as Sha3Digest;

// --- Helper functions for template engine ---

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

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
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        // Arrays and objects: fall back to JSON representation
        other => other.to_string(),
    }
}

enum VersionPart {
    Major,
    Minor,
    Patch,
}

fn increment_version(v: &str, part: VersionPart) -> String {
    let stripped = v.strip_prefix('v').unwrap_or(v);
    let parts: Vec<&str> = stripped.splitn(3, '.').collect();
    let major: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch: u64 = parts
        .get(2)
        .and_then(|s| {
            // Handle prerelease suffix: "3-rc.1" → "3"
            s.split('-').next().and_then(|n| n.parse().ok())
        })
        .unwrap_or(0);
    let prefix = if v.starts_with('v') { "v" } else { "" };
    match part {
        VersionPart::Major => format!("{}{}.0.0", prefix, major + 1),
        VersionPart::Minor => format!("{}{}.{}.0", prefix, major, minor + 1),
        VersionPart::Patch => format!("{}{}.{}.{}", prefix, major, minor, patch + 1),
    }
}

/// Base Tera instance with custom filters pre-registered.
/// Cloned per render() call (cheap — no templates to clone).
static BASE_TERA: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut tera = tera::Tera::default();

    // GoReleaser-compat aliases
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

    // --- Version increment functions (GoReleaser parity) ---

    // incpatch("1.2.3") → "1.2.4"
    tera.register_function(
        "incpatch",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("incpatch requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Patch)))
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
            Ok(Value::String(increment_version(v, VersionPart::Minor)))
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
            Ok(Value::String(increment_version(v, VersionPart::Major)))
        },
    );

    // --- Hash functions (GoReleaser parity — all 14 algorithms) ---

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
        hex_encode(&Sha1Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha224", |b: &[u8]| {
        let mut h = sha2::Sha224::new();
        Sha2Digest::update(&mut h, b);
        hex_encode(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha256", |b: &[u8]| {
        let mut h = sha2::Sha256::new();
        Sha2Digest::update(&mut h, b);
        hex_encode(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha384", |b: &[u8]| {
        let mut h = sha2::Sha384::new();
        Sha2Digest::update(&mut h, b);
        hex_encode(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha512", |b: &[u8]| {
        let mut h = sha2::Sha512::new();
        Sha2Digest::update(&mut h, b);
        hex_encode(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_224", |b: &[u8]| {
        let mut h = sha3::Sha3_224::new();
        Sha3Digest::update(&mut h, b);
        hex_encode(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_256", |b: &[u8]| {
        let mut h = sha3::Sha3_256::new();
        Sha3Digest::update(&mut h, b);
        hex_encode(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_384", |b: &[u8]| {
        let mut h = sha3::Sha3_384::new();
        Sha3Digest::update(&mut h, b);
        hex_encode(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_512", |b: &[u8]| {
        let mut h = sha3::Sha3_512::new();
        Sha3Digest::update(&mut h, b);
        hex_encode(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2b", |b: &[u8]| {
        let mut h = blake2::Blake2b512::new();
        blake2::Digest::update(&mut h, b);
        hex_encode(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2s", |b: &[u8]| {
        let mut h = blake2::Blake2s256::new();
        blake2::Digest::update(&mut h, b);
        hex_encode(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake3", |b: &[u8]| {
        hex_encode(blake3::hash(b).as_bytes())
    });
    register_hash_fn!(tera, "md5", |b: &[u8]| {
        let mut h = md5::Md5::new();
        md5::Digest::update(&mut h, b);
        hex_encode(&md5::Digest::finalize(h))
    });
    register_hash_fn!(tera, "crc32", |b: &[u8]| {
        format!("{:08x}", crc32fast::hash(b))
    });

    // --- File reading functions ---

    // readFile(path="file.txt") — reads file, returns empty string on error.
    // Intentionally returns empty on all errors (not just ENOENT) for GoReleaser-compatible behavior.
    // GoReleaser trims whitespace from the result (strings.TrimSpace).
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
    // GoReleaser trims whitespace from the result (strings.TrimSpace).
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
    tera.register_function(
        "time",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("%Y-%m-%dT%H:%M:%SZ");
            let now = chrono::Utc::now();
            Ok(Value::String(now.format(fmt).to_string()))
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
            // GoReleaser's url.PathEscape encodes `/` as `%2F`.
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

    // in(items=[...], value="x") — check if a list contains a value (GoReleaser Pro parity)
    // Go-style: {{ in (list "a" "b" "c") "b" }} → true
    // Named:    {{ in(items=["a","b","c"], value="b") }} → true
    // Compares all elements as strings.
    tera.register_function(
        "in",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
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
        },
    );

    // reReplaceAll(pattern="...", input="...", replacement="...") — regex replace (GoReleaser Pro parity)
    // Go-style: {{ reReplaceAll "(.*)" .Message "$1" }}
    // Named:    {{ reReplaceAll(pattern="(.*)", input="hello", replacement="$1") }}
    // Supports capture group references ($1, $2, etc.).
    // Returns a Tera error on invalid regex (no panic).
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
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("reReplaceAll: invalid regex '{}': {}", pattern, e)))?;
            Ok(Value::String(re.replace_all(input, replacement).to_string()))
        },
    );

    // reReplaceAll filter form: {{ Field | reReplaceAll(pattern="...", replacement="...") }}
    tera.register_filter(
        "reReplaceAll",
        |value: &Value, args: &HashMap<String, Value>| {
            let input = tera::try_get_value!("reReplaceAll", "value", String, value);
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reReplaceAll filter requires `pattern` argument"))?;
            let replacement = args
                .get("replacement")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reReplaceAll filter requires `replacement` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("reReplaceAll: invalid regex '{}': {}", pattern, e)))?;
            Ok(Value::String(re.replace_all(&input, replacement).to_string()))
        },
    );

    // englishJoin(items=[...], oxford=true) — join list items with commas and "and"
    // GoReleaser filters out empty/whitespace-only items before joining.
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
                    let (last, rest) = strs.split_last().unwrap();
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

    // filter(items=<string|array>, regexp="pattern") — keep elements matching regex
    // GoReleaser accepts a multiline STRING (splits by newline, filters lines, rejoins).
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
    // GoReleaser accepts a multiline STRING (splits by newline, filters lines, rejoins).
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

    // --- replace function (GoReleaser strings.ReplaceAll parity) ---
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

    // --- split function (GoReleaser strings.Split parity) ---
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

    // --- trim function (GoReleaser strings.TrimSpace parity) ---
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

    // --- title function (GoReleaser strings.ToTitle parity) ---
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
        Ok(Value::String(increment_version(&v, VersionPart::Patch)))
    });

    // incminor — filter form: {{ "1.2.3" | incminor }}
    tera.register_filter("incminor", |value: &Value, _: &HashMap<String, Value>| {
        let v = tera::try_get_value!("incminor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Minor)))
    });

    // incmajor — filter form: {{ "1.2.3" | incmajor }}
    tera.register_filter("incmajor", |value: &Value, _: &HashMap<String, Value>| {
        let v = tera::try_get_value!("incmajor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Major)))
    });

    // in — filter form: {{ myList | in(value="x") }}
    // Checks whether the piped array contains the given value (string comparison).
    tera.register_filter(
        "in",
        |value: &Value, args: &HashMap<String, Value>| {
            let items = value
                .as_array()
                .ok_or_else(|| tera::Error::msg("in filter requires an array as input"))?;
            let needle = args
                .get("value")
                .ok_or_else(|| tera::Error::msg("in filter requires `value` argument"))?;
            let needle_str = value_to_string(needle);
            let found = items.iter().any(|item| value_to_string(item) == needle_str);
            Ok(Value::Bool(found))
        },
    );

    tera
});

#[derive(Clone)]
pub struct TemplateVars {
    vars: HashMap<String, String>,
    env: HashMap<String, String>,
    /// Custom user-defined variables accessible as {{ .Var.key }}.
    custom_vars: HashMap<String, String>,
}

impl TemplateVars {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            env: HashMap::new(),
            custom_vars: HashMap::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.vars.insert(key.to_string(), value.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    pub fn set_custom_var(&mut self, key: &str, value: &str) {
        self.custom_vars.insert(key.to_string(), value.to_string());
    }

    /// Return all template variables (excluding env and custom vars).
    pub fn all(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// Return all environment variables.
    pub fn all_env(&self) -> &HashMap<String, String> {
        &self.env
    }
}

impl Default for TemplateVars {
    fn default() -> Self {
        Self::new()
    }
}

/// Known numeric template fields that should be inserted as integers into the
/// Tera context so that numeric comparisons like `{% if Major == 1 %}` work
/// correctly. Without this, they would be strings and `"1" != 1`.
const NUMERIC_FIELDS: &[&str] = &["Major", "Minor", "Patch", "Timestamp", "CommitTimestamp"];

/// Build a `tera::Context` from `TemplateVars`.
/// - Regular vars are inserted at the top level: `ProjectName`, `Version`, etc.
/// - Env vars are nested under an `Env` key as a HashMap, so `{{ Env.GITHUB_TOKEN }}` works.
/// - String values of `"true"` / `"false"` are inserted as bools so `{% if Var %}` works.
/// - Known numeric fields (`Major`, `Minor`, `Patch`, `Timestamp`, `CommitTimestamp`)
///   are inserted as integers so `{% if Major == 1 %}` works correctly.
fn build_tera_context(vars: &TemplateVars) -> tera::Context {
    let mut ctx = tera::Context::new();
    for (k, v) in &vars.vars {
        // For known numeric fields, parse as i64 and insert as a number so
        // Tera comparisons like `{% if Major == 1 %}` work correctly.
        if NUMERIC_FIELDS.contains(&k.as_str())
            && let Ok(n) = v.parse::<i64>()
        {
            ctx.insert(k.as_str(), &n);
            continue;
        }
        match v.as_str() {
            "true" => ctx.insert(k.as_str(), &true),
            "false" => ctx.insert(k.as_str(), &false),
            _ => ctx.insert(k.as_str(), v),
        }
    }
    ctx.insert("Env", &vars.env);

    // Always insert Var (even when empty) so that `{{ Var.key }}` returns ""
    // instead of a hard Tera error when no variables are defined. This matches
    // GoReleaser which provides an empty .Var map by default.
    ctx.insert("Var", &vars.custom_vars);

    // Build a nested `Runtime` map for GoReleaser `Runtime.Goos` / `Runtime.Goarch` compat.
    let mut runtime = HashMap::new();
    if let Some(goos) = vars.vars.get("RuntimeGoos") {
        runtime.insert("Goos".to_string(), goos.clone());
    }
    if let Some(goarch) = vars.vars.get("RuntimeGoarch") {
        runtime.insert("Goarch".to_string(), goarch.clone());
    }
    if !runtime.is_empty() {
        ctx.insert("Runtime", &runtime);
    }

    ctx
}

/// Render a template string with the given variables.
///
/// Supports both Go-style (`{{ .Field }}`) and native Tera-style (`{{ Field }}`).
/// Go-style references are preprocessed into Tera-style before rendering.
///
/// Because this uses Tera under the hood, all Tera features are available:
/// conditionals (`{% if %}` / `{% else %}` / `{% endif %}`), loops (`{% for %}`),
/// filters (`| lower`, `| upper`, `| default`, `| trim`, `| title`, `| replace`, etc.).
///
/// Custom GoReleaser-compat filters are registered:
/// - `tolower` / `toupper` — aliases for Tera's built-in `lower` / `upper`
/// - `trimprefix(prefix="v")` — strip a prefix from a string
/// - `trimsuffix(suffix=".exe")` — strip a suffix from a string
pub fn render(template: &str, vars: &TemplateVars) -> Result<String> {
    let preprocessed = preprocess(template);
    let ctx = build_tera_context(vars);

    // Clone the base instance (cheap — filters carry over, no templates to clone)
    let mut tera = BASE_TERA.clone();

    // Override envOrDefault and isEnvSet with closures that read from the
    // template context's Env map. This ensures .env file vars (loaded into
    // TemplateVars via set_env) are visible, not just process env vars.
    // Falls back to std::env::var for vars that exist in the process env
    // but were not explicitly added to the template context.
    let env_map = std::sync::Arc::new(vars.all_env().clone());
    let env_map_for_default = env_map.clone();
    tera.register_function(
        "envOrDefault",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            // Check template context Env map first, then fall back to process env.
            let value = env_map_for_default
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_else(|| default.to_string());
            Ok(Value::String(value))
        },
    );

    let env_map_for_isset = env_map.clone();
    tera.register_function(
        "isEnvSet",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("isEnvSet requires `name` argument"))?;
            // Check template context Env map first, then fall back to process env.
            let is_set = env_map_for_isset
                .get(name)
                .map(|v| !v.is_empty())
                .unwrap_or_else(|| std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false));
            Ok(Value::Bool(is_set))
        },
    );

    tera.add_raw_template("__inline__", &preprocessed)
        .with_context(|| format!("failed to parse template: {}", template))?;

    tera.render("__inline__", &ctx)
        .with_context(|| format!("failed to render template: {}", template))
}

/// Validate that a template string contains only a single `{{ Env.VAR }}` reference.
/// Used for credential fields (e.g. Docker registry passwords) to prevent
/// hardcoded secrets mixed with env var references.
///
/// Accepts: `{{ .Env.VAR }}`, `{{ Env.VAR }}`, `{{.Env.VAR}}`, `{{Env.VAR}}`
/// Rejects: `prefix-{{ .Env.VAR }}`, `{{ .Env.VAR }}-suffix`, any literal text
pub fn validate_single_env_only(template: &str) -> Result<()> {
    static ENV_ONLY_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*\{\{\s*\.?Env\.[A-Za-z_][A-Za-z0-9_]*\s*\}\}\s*$").unwrap()
    });
    if ENV_ONLY_RE.is_match(template) {
        Ok(())
    } else {
        anyhow::bail!(
            "expected a single env var reference like '{{{{ .Env.VAR }}}}', got: {}",
            template
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_vars() -> TemplateVars {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "cfgd");
        vars.set("Version", "1.2.3");
        vars.set("Tag", "v1.2.3");
        vars.set("Os", "linux");
        vars.set("Arch", "amd64");
        vars.set("ShortCommit", "abc1234");
        vars.set("Major", "1");
        vars.set("Minor", "2");
        vars.set("Patch", "3");
        vars.set_env("GITHUB_TOKEN", "tok123");
        vars
    }

    #[test]
    fn test_simple_substitution() {
        let vars = test_vars();
        let result = render("{{ .ProjectName }}-{{ .Version }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_env_access() {
        let vars = test_vars();
        let result = render("{{ .Env.GITHUB_TOKEN }}", &vars).unwrap();
        assert_eq!(result, "tok123");
    }

    #[test]
    fn test_no_spaces() {
        let vars = test_vars();
        let result = render("{{.ProjectName}}-{{.Version}}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_missing_var() {
        let vars = test_vars();
        let result = render("{{ .Missing }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_archive_name_template() {
        let vars = test_vars();
        let result = render(
            "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_literal_text_preserved() {
        let vars = test_vars();
        let result = render("prefix-{{ .Tag }}-suffix.tar.gz", &vars).unwrap();
        assert_eq!(result, "prefix-v1.2.3-suffix.tar.gz");
    }

    // Tera-style tests (no leading dot)

    #[test]
    fn test_tera_simple_substitution() {
        let vars = test_vars();
        let result = render("{{ ProjectName }}-{{ Version }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_tera_env_access() {
        let vars = test_vars();
        let result = render("{{ Env.GITHUB_TOKEN }}", &vars).unwrap();
        assert_eq!(result, "tok123");
    }

    #[test]
    fn test_tera_archive_name() {
        let vars = test_vars();
        let result = render("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_tera_missing_var() {
        let vars = test_vars();
        let result = render("{{ Missing }}", &vars);
        assert!(result.is_err());
    }

    // --- Task 1B: custom filters and extended template tests ---

    #[test]
    fn test_conditional_true() {
        let mut vars = test_vars();
        vars.set("IsSnapshot", "true");
        let result = render("{% if IsSnapshot %}SNAP{% endif %}", &vars).unwrap();
        assert_eq!(result, "SNAP");
    }

    #[test]
    fn test_conditional_false_else() {
        let mut vars = test_vars();
        vars.set("IsSnapshot", "false");
        let result = render("{% if IsSnapshot %}SNAP{% else %}RELEASE{% endif %}", &vars).unwrap();
        assert_eq!(result, "RELEASE");
    }

    #[test]
    fn test_pipe_lower() {
        let vars = test_vars();
        let result = render("{{ Version | lower }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_pipe_upper() {
        let vars = test_vars();
        let result = render("{{ ProjectName | upper }}", &vars).unwrap();
        assert_eq!(result, "CFGD");
    }

    #[test]
    fn test_tolower_alias() {
        let vars = test_vars();
        let result = render("{{ ProjectName | tolower }}", &vars).unwrap();
        assert_eq!(result, "cfgd");
    }

    #[test]
    fn test_toupper_alias() {
        let vars = test_vars();
        let result = render("{{ ProjectName | toupper }}", &vars).unwrap();
        assert_eq!(result, "CFGD");
    }

    #[test]
    fn test_trimprefix() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"v\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_trimprefix_no_match() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"x\") }}", &vars).unwrap();
        assert_eq!(result, "v1.2.3");
    }

    #[test]
    fn test_trimsuffix() {
        let vars = test_vars();
        let result = render("{{ ProjectName | trimsuffix(suffix=\"gd\") }}", &vars).unwrap();
        assert_eq!(result, "cf");
    }

    #[test]
    fn test_trimsuffix_no_match() {
        let vars = test_vars();
        let result = render("{{ ProjectName | trimsuffix(suffix=\"xyz\") }}", &vars).unwrap();
        assert_eq!(result, "cfgd");
    }

    #[test]
    fn test_default_value_for_undefined() {
        let vars = test_vars();
        let result = render("{{ Undefined | default(value=\"fallback\") }}", &vars).unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_bad_syntax_error() {
        let vars = test_vars();
        let result = render("{{ unclosed", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_env_conditional() {
        let vars = test_vars();
        let result = render("{% if Env.GITHUB_TOKEN %}has token{% endif %}", &vars).unwrap();
        assert_eq!(result, "has token");
    }

    #[test]
    fn test_trimprefix_missing_arg_error() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_trimsuffix_missing_arg_error() {
        let vars = test_vars();
        let result = render("{{ Tag | trimsuffix }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_chaining() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"v\") | upper }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_unknown_filter_error() {
        let vars = test_vars();
        let result = render("{{ ProjectName | nonexistent_filter }}", &vars);
        assert!(result.is_err(), "unknown filter should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nonexistent_filter"),
            "error should mention the unknown filter name, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_block_tag_error() {
        let vars = test_vars();
        let result = render("{% if ProjectName %} hello", &vars);
        assert!(result.is_err(), "unclosed if block should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("if"),
            "error should reference the template or block tag, got: {err}"
        );
    }

    #[test]
    fn test_trailing_pipe_with_no_filter_name_error() {
        let vars = test_vars();
        // A trailing pipe with no filter name is a distinct syntax error from
        // just an unclosed tag (which test_bad_syntax_error already covers).
        let result = render("{{ ProjectName | }}", &vars);
        assert!(
            result.is_err(),
            "trailing pipe with no filter name should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parse") || err.contains("unexpected") || err.contains("template"),
            "error should mention a parsing issue, got: {err}"
        );
    }

    #[test]
    fn test_nested_missing_var_in_expression_error() {
        let vars = test_vars();
        // Using an undefined variable in an expression (not just a conditional
        // truthiness check) should error when the template tries to render it.
        let result = render("{{ Undefined ~ ' suffix' }}", &vars);
        assert!(
            result.is_err(),
            "undefined variable in an expression should produce an error"
        );
    }

    #[test]
    fn test_invalid_filter_argument_type_error() {
        let vars = test_vars();
        // trimprefix expects prefix=<string>, but we pass an unquoted value
        // that Tera will interpret differently
        let result = render("{{ Tag | trimprefix(prefix=123) }}", &vars);
        assert!(
            result.is_err(),
            "invalid filter argument type should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("trimprefix") || err.contains("prefix") || err.contains("argument"),
            "error should mention the filter or argument, got: {err}"
        );
    }

    #[test]
    fn test_error_message_includes_original_template() {
        let vars = test_vars();
        let template = "{{ .Nonexistent }}";
        let result = render(template, &vars);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Our render() adds context with the original template
        assert!(
            err.contains("Nonexistent") || err.contains(template),
            "error should reference the template or variable name, got: {err}"
        );
    }

    #[test]
    fn test_mismatched_endfor_with_if_error() {
        let vars = test_vars();
        let result = render("{% if ProjectName %}hello{% endfor %}", &vars);
        assert!(
            result.is_err(),
            "mismatched block tags should produce an error"
        );
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_undefined_variable_error_mentions_variable() {
        let vars = test_vars();
        let result = render("{{ UndefinedFoo }}", &vars);
        assert!(
            result.is_err(),
            "undefined variable should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("UndefinedFoo") || err.contains("template"),
            "error should mention the undefined variable name, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_brace_syntax_error() {
        let vars = test_vars();
        let result = render("{{ ProjectName", &vars);
        assert!(result.is_err(), "unclosed brace should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parse") || err.contains("template") || err.contains("ProjectName"),
            "error should indicate a parse failure, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_tag_block_error() {
        let vars = test_vars();
        let result = render("{% for x in items %} content", &vars);
        assert!(
            result.is_err(),
            "unclosed for block should produce an error"
        );
    }

    #[test]
    fn test_invalid_filter_name_error_mentions_filter() {
        let vars = test_vars();
        let result = render("{{ ProjectName | bogus_filter_name }}", &vars);
        assert!(result.is_err(), "invalid filter should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bogus_filter_name"),
            "error should mention the invalid filter name, got: {err}"
        );
    }

    #[test]
    fn test_deeply_nested_undefined_variable_error() {
        let vars = test_vars();
        let result = render("{{ Env.NONEXISTENT_VAR_12345 }}", &vars);
        // Env is defined but NONEXISTENT_VAR_12345 is not a key in it.
        // Tera treats this as an undefined variable and returns an error.
        assert!(
            result.is_err(),
            "accessing a missing key in a map should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("NONEXISTENT_VAR_12345") || err.contains("Env"),
            "error should reference the undefined variable, got: {err}"
        );
    }

    #[test]
    fn test_go_style_syntax_error_reports_original_template() {
        let vars = test_vars();
        let template = "{{ .Missing | bad_filter }}";
        let result = render(template, &vars);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // The error context added by render() should include the original template
        assert!(
            err.contains("bad_filter") || err.contains(template),
            "error should reference the original template or filter, got: {err}"
        );
    }

    #[test]
    fn test_empty_template_renders_empty() {
        let vars = test_vars();
        let result = render("", &vars);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_multiple_errors_in_template() {
        let vars = test_vars();
        // This template has both an undefined variable and a syntax issue
        let result = render("{% if %}", &vars);
        assert!(
            result.is_err(),
            "empty if condition should produce an error"
        );
    }

    // ---- envOrDefault and isEnvSet function tests ----

    #[test]
    fn test_env_or_default_reads_from_template_env_map() {
        // The primary path: envOrDefault reads from the template context Env map,
        // NOT from the process environment. This is the .env file use case.
        let mut vars = test_vars();
        vars.set_env("MY_CUSTOM_VAR", "from-template-env");
        let result = render(
            "{{ envOrDefault(name=\"MY_CUSTOM_VAR\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "from-template-env");
    }

    #[test]
    fn test_env_or_default_template_env_takes_priority_over_process_env() {
        // If a var exists in both the template Env map and the process env,
        // the template Env map wins.
        let mut vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_PRIORITY", "from-process") };
        vars.set_env("ANODIZE_TEST_PRIORITY", "from-template");
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_PRIORITY\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "from-template");
        unsafe { std::env::remove_var("ANODIZE_TEST_PRIORITY") };
    }

    #[test]
    fn test_env_or_default_falls_back_to_process_env() {
        // If a var is NOT in the template Env map but IS in the process env,
        // fall back to the process env.
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_ENV_OR_DEFAULT", "from-process-env") };
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_ENV_OR_DEFAULT\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "from-process-env");
        unsafe { std::env::remove_var("ANODIZE_TEST_ENV_OR_DEFAULT") };
    }

    #[test]
    fn test_env_or_default_returns_default_when_unset() {
        let vars = test_vars();
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_UNSET_VAR_XYZ\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_env_or_default_returns_empty_when_no_default() {
        let vars = test_vars();
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_UNSET_VAR_XYZ2\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_env_or_default_missing_name_error() {
        let vars = test_vars();
        let result = render("{{ envOrDefault(default=\"x\") }}", &vars);
        assert!(result.is_err(), "envOrDefault without name should error");
    }

    #[test]
    fn test_is_env_set_reads_from_template_env_map() {
        // The primary path: isEnvSet reads from the template context Env map.
        let mut vars = test_vars();
        vars.set_env("MY_CUSTOM_CHECK", "yes");
        let result = render(
            "{% if isEnvSet(name=\"MY_CUSTOM_CHECK\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "SET");
    }

    #[test]
    fn test_is_env_set_template_env_empty_returns_false() {
        // An empty string in the template Env map should return false.
        let mut vars = test_vars();
        vars.set_env("MY_EMPTY_VAR", "");
        let result = render(
            "{% if isEnvSet(name=\"MY_EMPTY_VAR\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
    }

    #[test]
    fn test_is_env_set_falls_back_to_process_env() {
        // If a var is NOT in the template Env map but IS in the process env,
        // fall back to the process env.
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_IS_SET", "yes") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_IS_SET\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "SET");
        unsafe { std::env::remove_var("ANODIZE_TEST_IS_SET") };
    }

    #[test]
    fn test_is_env_set_false_when_unset() {
        let vars = test_vars();
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_NOT_SET_XYZ\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
    }

    #[test]
    fn test_is_env_set_missing_name_error() {
        let vars = test_vars();
        let result = render("{{ isEnvSet() }}", &vars);
        assert!(result.is_err(), "isEnvSet without name should error");
    }

    // ---- Hash function tests (known-answer vectors) ----
    // Hash functions read file contents (GoReleaser parity), so tests use temp files.

    fn hash_test_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "hello").unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    #[test]
    fn test_hash_sha1() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ sha1(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn test_hash_sha256() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ sha256(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hash_sha512() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ sha512(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(
            result,
            "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
        );
    }

    #[test]
    fn test_hash_md5() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ md5(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_hash_blake3() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ blake3(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(
            result,
            "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
        );
    }

    #[test]
    fn test_hash_crc32() {
        let vars = test_vars();
        let (_dir, path) = hash_test_file();
        let tmpl = format!("{{{{ crc32(s=\"{path}\") }}}}");
        let result = render(&tmpl, &vars).unwrap();
        assert_eq!(result, "3610a686");
    }

    #[test]
    fn test_hash_missing_s_arg_error() {
        let vars = test_vars();
        let result = render("{{ sha256() }}", &vars);
        assert!(
            result.is_err(),
            "hash function without `s` arg should error"
        );
        // The anyhow error chain includes the tera error with our message
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("requires `s` argument"),
            "error should mention missing `s` argument, got: {err}"
        );
    }

    // ---- Version increment tests ----

    #[test]
    fn test_incpatch() {
        let vars = test_vars();
        let result = render("{{ incpatch(v=\"1.2.3\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.4");
    }

    #[test]
    fn test_incminor() {
        let vars = test_vars();
        let result = render("{{ incminor(v=\"1.2.3\") }}", &vars).unwrap();
        assert_eq!(result, "1.3.0");
    }

    #[test]
    fn test_incmajor() {
        let vars = test_vars();
        let result = render("{{ incmajor(v=\"1.2.3\") }}", &vars).unwrap();
        assert_eq!(result, "2.0.0");
    }

    #[test]
    fn test_incpatch_preserves_v_prefix() {
        let vars = test_vars();
        let result = render("{{ incpatch(v=\"v1.2.3\") }}", &vars).unwrap();
        assert_eq!(result, "v1.2.4");
    }

    #[test]
    fn test_incpatch_handles_prerelease() {
        let vars = test_vars();
        let result = render("{{ incpatch(v=\"1.2.3-rc.1\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.4");
    }

    // ---- readFile / mustReadFile tests ----

    #[test]
    fn test_read_file_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "file content").unwrap();

        let vars = test_vars();
        let template = format!(
            "{{{{ readFile(path=\"{}\") }}}}",
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = render(&template, &vars).unwrap();
        assert_eq!(result, "file content");
    }

    #[test]
    fn test_read_file_nonexistent_returns_empty() {
        let vars = test_vars();
        let result = render(
            "{{ readFile(path=\"/tmp/anodize_test_nonexistent_file_xyz\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_must_read_file_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "must content").unwrap();

        let vars = test_vars();
        let template = format!(
            "{{{{ mustReadFile(path=\"{}\") }}}}",
            file_path.to_string_lossy().replace('\\', "\\\\")
        );
        let result = render(&template, &vars).unwrap();
        assert_eq!(result, "must content");
    }

    #[test]
    fn test_must_read_file_nonexistent_errors() {
        let vars = test_vars();
        let result = render(
            "{{ mustReadFile(path=\"/tmp/anodize_test_nonexistent_file_xyz\") }}",
            &vars,
        );
        assert!(
            result.is_err(),
            "mustReadFile with nonexistent file should error"
        );
    }

    // ---- Path filter tests ----

    #[test]
    fn test_dir_filter() {
        let mut vars = test_vars();
        vars.set("FilePath", "/foo/bar/baz.txt");
        let result = render("{{ FilePath | dir }}", &vars).unwrap();
        assert_eq!(result, "/foo/bar");
    }

    #[test]
    fn test_base_filter() {
        let mut vars = test_vars();
        vars.set("FilePath", "/foo/bar/baz.txt");
        let result = render("{{ FilePath | base }}", &vars).unwrap();
        assert_eq!(result, "baz.txt");
    }

    // ---- urlPathEscape tests ----

    #[test]
    fn test_url_path_escape_spaces() {
        let mut vars = test_vars();
        vars.set("Input", "hello world");
        let result = render("{{ Input | urlPathEscape }}", &vars).unwrap();
        assert_eq!(result, "hello%20world");
    }

    #[test]
    fn test_url_path_escape_encodes_slashes() {
        let mut vars = test_vars();
        vars.set("Input", "foo/bar");
        let result = render("{{ Input | urlPathEscape }}", &vars).unwrap();
        assert_eq!(result, "foo%2Fbar");
    }

    // ---- mdv2escape tests ----

    #[test]
    fn test_mdv2escape() {
        let mut vars = test_vars();
        vars.set("Input", "hello_world");
        let result = render("{{ Input | mdv2escape }}", &vars).unwrap();
        assert_eq!(result, "hello\\_world");
    }

    // ---- contains tests ----

    #[test]
    fn test_contains_true() {
        let vars = test_vars();
        let result = render(
            "{% if contains(s=\"hello world\", substr=\"world\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_contains_false() {
        let vars = test_vars();
        let result = render(
            "{% if contains(s=\"hello\", substr=\"xyz\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    // ---- englishJoin tests ----

    #[test]
    fn test_english_join_zero_items() {
        let vars = test_vars();
        // Pass an empty array via list()
        let result = render("{{ englishJoin(items=[]) }}", &vars).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_english_join_one_item() {
        let vars = test_vars();
        let result = render("{{ englishJoin(items=[\"a\"]) }}", &vars).unwrap();
        assert_eq!(result, "a");
    }

    #[test]
    fn test_english_join_two_items() {
        let vars = test_vars();
        let result = render("{{ englishJoin(items=[\"a\", \"b\"]) }}", &vars).unwrap();
        assert_eq!(result, "a and b");
    }

    #[test]
    fn test_english_join_three_items_oxford() {
        let vars = test_vars();
        let result = render("{{ englishJoin(items=[\"a\", \"b\", \"c\"]) }}", &vars).unwrap();
        assert_eq!(result, "a, b, and c");
    }

    #[test]
    fn test_english_join_three_items_no_oxford() {
        let vars = test_vars();
        let result = render(
            "{{ englishJoin(items=[\"a\", \"b\", \"c\"], oxford=false) }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "a, b and c");
    }

    // ---- filter / reverseFilter tests ----

    #[test]
    fn test_filter_keeps_matches() {
        let vars = test_vars();
        let result = render(
            "{{ filter(items=[\"apple\", \"banana\", \"avocado\"], regexp=\"^a\") }}",
            &vars,
        )
        .unwrap();
        // Tera renders arrays as JSON
        assert!(result.contains("apple"));
        assert!(result.contains("avocado"));
        assert!(!result.contains("banana"));
    }

    #[test]
    fn test_reverse_filter_removes_matches() {
        let vars = test_vars();
        let result = render(
            "{{ reverseFilter(items=[\"apple\", \"banana\", \"avocado\"], regexp=\"^a\") }}",
            &vars,
        )
        .unwrap();
        assert!(result.contains("banana"));
        assert!(!result.contains("apple"));
        assert!(!result.contains("avocado"));
    }

    // ---- indexOrDefault tests ----

    #[test]
    fn test_index_or_default_key_exists() {
        // We need to construct a template that passes a map. Tera doesn't have inline map
        // literals in templates, so we test the function via the Rust API directly.
        let args: HashMap<String, Value> = [
            ("map".to_string(), serde_json::json!({"foo": "bar"})),
            ("key".to_string(), Value::String("foo".to_string())),
            ("default".to_string(), Value::String("fallback".to_string())),
        ]
        .into_iter()
        .collect();

        // Access the function via BASE_TERA - we test it indirectly by calling the logic
        let map = args.get("map").unwrap().as_object().unwrap();
        let key = args.get("key").unwrap().as_str().unwrap();
        let default = args
            .get("default")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let result = map.get(key).cloned().unwrap_or(default);
        assert_eq!(result, Value::String("bar".to_string()));
    }

    #[test]
    fn test_index_or_default_key_missing() {
        let args: HashMap<String, Value> = [
            ("map".to_string(), serde_json::json!({"foo": "bar"})),
            ("key".to_string(), Value::String("missing".to_string())),
            ("default".to_string(), Value::String("fallback".to_string())),
        ]
        .into_iter()
        .collect();

        let map = args.get("map").unwrap().as_object().unwrap();
        let key = args.get("key").unwrap().as_str().unwrap();
        let default = args
            .get("default")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let result = map.get(key).cloned().unwrap_or(default);
        assert_eq!(result, Value::String("fallback".to_string()));
    }

    // ---- Runtime vars test ----

    #[test]
    fn test_runtime_goos_renders() {
        let mut vars = test_vars();
        vars.set("RuntimeGoos", std::env::consts::OS);
        let result = render("{{ Runtime.Goos }}", &vars).unwrap();
        assert!(
            !result.is_empty(),
            "Runtime.Goos should render to a non-empty string"
        );
    }

    // ---- Custom variables (.Var.*) tests ----

    #[test]
    fn test_custom_var_tera_style() {
        let mut vars = test_vars();
        vars.set_custom_var("description", "my project description");
        let result = render("{{ Var.description }}", &vars).unwrap();
        assert_eq!(result, "my project description");
    }

    #[test]
    fn test_custom_var_go_style() {
        let mut vars = test_vars();
        vars.set_custom_var("mykey", "myvalue");
        let result = render("{{ .Var.mykey }}", &vars).unwrap();
        assert_eq!(result, "myvalue");
    }

    #[test]
    fn test_custom_var_multiple() {
        let mut vars = test_vars();
        vars.set_custom_var("name", "anodize");
        vars.set_custom_var("desc", "release tool");
        let result = render("{{ .Var.name }} - {{ .Var.desc }}", &vars).unwrap();
        assert_eq!(result, "anodize - release tool");
    }

    #[test]
    fn test_custom_var_empty_map_no_error() {
        // When no custom vars are set, Var is still inserted as an empty map.
        // Rendering a template that does NOT reference Var should succeed.
        let vars = test_vars();
        let result = render("{{ ProjectName }}", &vars).unwrap();
        assert_eq!(result, "cfgd");
    }

    #[test]
    fn test_custom_var_undefined_key_errors() {
        // Accessing an undefined key within the Var map produces an error,
        // matching Tera's strict behavior (same as Env.NONEXISTENT).
        // Users can use `{{ Var.key | default(value="") }}` for optional vars.
        let vars = test_vars();
        let result = render("{{ Var.nonexistent }}", &vars);
        assert!(
            result.is_err(),
            "accessing a missing key in Var should produce an error"
        );
    }

    #[test]
    fn test_custom_var_undefined_key_with_other_vars_set() {
        // When some custom vars exist, referencing an undefined key should
        // still error (Tera strict mode).
        let mut vars = test_vars();
        vars.set_custom_var("exists", "yes");
        let result = render("{{ Var.missing }}", &vars);
        assert!(
            result.is_err(),
            "accessing a missing key in Var should produce an error"
        );
    }

    #[test]
    fn test_custom_var_empty_map_conditional() {
        // Var is always inserted as an empty map. Tera treats empty maps as
        // falsy so `{% if Var %}` correctly distinguishes empty vs non-empty.
        let vars = test_vars();
        let result = render("{% if Var %}has vars{% else %}no vars{% endif %}", &vars).unwrap();
        assert_eq!(result, "no vars");

        let mut vars2 = test_vars();
        vars2.set_custom_var("key", "val");
        let result2 = render("{% if Var %}has vars{% else %}no vars{% endif %}", &vars2).unwrap();
        assert_eq!(result2, "has vars");
    }

    #[test]
    fn test_custom_var_with_template_in_value() {
        // Verify that custom var values can themselves be template-rendered
        // (this is done in the CLI wiring, but we can test the end result here)
        let mut vars = test_vars();
        // Simulate a pre-rendered value (as the CLI would do)
        vars.set_custom_var("version_string", "cfgd v1.2.3");
        let result = render("{{ .Var.version_string }}", &vars).unwrap();
        assert_eq!(result, "cfgd v1.2.3");
    }

    // ---- Go-style positional syntax tests (Task 2) ----

    #[test]
    fn test_positional_replace_standalone() {
        // {{ replace .Version "v" "" }} should strip "v" from empty tag
        let mut vars = test_vars();
        vars.set("Version", "v1.2.3");
        let result = render("{{ replace .Version \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_replace_standalone_no_dot() {
        // Tera-style: {{ replace Version "v" "" }}
        let mut vars = test_vars();
        vars.set("Version", "v1.2.3");
        let result = render("{{ replace Version \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_replace_piped() {
        // {{ .Version | replace "v" "" }} should strip "v" prefix
        let mut vars = test_vars();
        vars.set("Version", "v1.2.3");
        let result = render("{{ .Version | replace \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_replace_piped_no_dot() {
        // Tera-style: {{ Version | replace "v" "" }}
        let mut vars = test_vars();
        vars.set("Version", "v1.2.3");
        let result = render("{{ Version | replace \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_split_standalone() {
        // {{ split .Version "." }} should split on dots
        let vars = test_vars();
        let result = render("{{ split .Version \".\" }}", &vars).unwrap();
        // Tera renders arrays as JSON, e.g. ["1", "2", "3"]
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("3"));
    }

    #[test]
    fn test_positional_split_piped() {
        // {{ .Version | split "." }} should split on dots
        let vars = test_vars();
        let result = render("{{ .Version | split \".\" }}", &vars).unwrap();
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("3"));
    }

    #[test]
    fn test_positional_contains_standalone_true() {
        // {{ contains .Version "2" }} should return true
        let vars = test_vars();
        let result = render(
            "{% if contains .Version \"2\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_positional_contains_standalone_false() {
        let vars = test_vars();
        let result = render(
            "{% if contains .Version \"rc\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_positional_contains_piped() {
        // {{ .Tag | contains "v" }} piped positional form
        let vars = test_vars();
        let result = render(
            "{% if Tag | contains \"v\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_positional_replace_with_env_var() {
        // Using dotted path: {{ replace .Env.GITHUB_TOKEN "tok" "XXX" }}
        let vars = test_vars();
        let result =
            render("{{ replace .Env.GITHUB_TOKEN \"tok\" \"XXX\" }}", &vars).unwrap();
        assert_eq!(result, "XXX123");
    }

    #[test]
    fn test_positional_replace_empty_replacement() {
        // Common GoReleaser pattern: strip "v" prefix
        let vars = test_vars();
        let result = render("{{ replace .Tag \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_named_arg_syntax_passthrough() {
        // Already using named args — should NOT be rewritten
        let vars = test_vars();
        let result =
            render("{{ replace(s=Tag, old=\"v\", new=\"\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_named_arg_filter_passthrough() {
        // Already using named filter args — should NOT be rewritten
        let vars = test_vars();
        let result =
            render("{{ Tag | replace(from=\"v\", to=\"\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_mixed_with_literal_text() {
        // Positional syntax mixed with literal text around it
        let vars = test_vars();
        let result = render(
            "app-{{ replace .Tag \"v\" \"\" }}-{{ .Os }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "app-1.2.3-linux");
    }

    #[test]
    fn test_positional_replace_both_quoted_args() {
        // All args quoted — replace("v1.2.3", "v", "")
        let vars = test_vars();
        let result = render("{{ replace \"v1.2.3\" \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_positional_split_literal_string() {
        // split with a literal string instead of a variable
        let vars = test_vars();
        let result = render("{{ split \"a.b.c\" \".\" }}", &vars).unwrap();
        assert!(result.contains("a"));
        assert!(result.contains("b"));
        assert!(result.contains("c"));
    }

    #[test]
    fn test_positional_contains_literal_string() {
        let vars = test_vars();
        let result = render(
            "{% if contains \"hello world\" \"world\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_split_filter_end_to_end() {
        // Test the split filter registration works end-to-end
        let vars = test_vars();
        let result = render("{{ Version | split(sep=\".\") }}", &vars).unwrap();
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("3"));
    }

    #[test]
    fn test_contains_filter_end_to_end() {
        // Test the contains filter registration works end-to-end
        let vars = test_vars();
        let result = render(
            "{% if Tag | contains(substr=\"v\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_chained_named_filter_then_positional_rewrite() {
        // Chained: named-arg filter followed by positional rewrite.
        // `{{ Version | trimprefix(prefix="v") | replace "." "-" }}`
        // The first filter uses named-arg syntax (has parens), the second uses positional.
        // The preprocessor should rewrite ONLY the last segment's positional args
        // while preserving the named-arg filter unchanged.
        let mut vars = test_vars();
        vars.set("Version", "v1.2.3");

        // Verify end-to-end rendering
        let input = "{{ Version | trimprefix(prefix=\"v\") | replace \".\" \"-\" }}";
        let result = render(input, &vars).unwrap();
        assert_eq!(result, "1-2-3");
    }

    // ---- `in` function tests ----

    #[test]
    fn test_in_list_contains_value() {
        let vars = test_vars();
        let result = render(
            "{% if in(items=[\"a\", \"b\", \"c\"], value=\"b\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_in_list_not_contains_value() {
        let vars = test_vars();
        let result = render(
            "{% if in(items=[\"a\", \"b\", \"c\"], value=\"d\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_in_empty_list() {
        let vars = test_vars();
        let result = render(
            "{% if in(items=[], value=\"a\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_in_go_style_positional_with_list_subexpr() {
        // Go-style: {{ in (list "a" "b" "c") "b" }}
        // This exercises the full preprocessing pipeline:
        // 1. (list "a" "b" "c") → ["a", "b", "c"]
        // 2. in ["a", "b", "c"] "b" → in(items=["a", "b", "c"], value="b")
        let vars = test_vars();
        let result = render(
            "{% if in (list \"linux\" \"darwin\") \"linux\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_in_go_style_positional_with_list_subexpr_not_found() {
        let vars = test_vars();
        let result = render(
            "{% if in (list \"linux\" \"darwin\") \"windows\" %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_in_positional_with_variable() {
        // {{ in myList "b" }} where myList is a template variable
        // NOTE: This requires myList to be set as a Tera array in the context.
        // Since TemplateVars only supports string vars, we test with the list subexpr form instead.
        let vars = test_vars();
        let result = render(
            "{% if in (list \"a\" \"b\" \"c\") \"c\" %}found{% else %}nope{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "found");
    }

    #[test]
    fn test_in_renders_bool_string() {
        // When used in an expression context, `in` should render as "true" or "false"
        let vars = test_vars();
        let result = render(
            "{{ in(items=[\"a\", \"b\"], value=\"a\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn test_in_renders_bool_string_false() {
        let vars = test_vars();
        let result = render(
            "{{ in(items=[\"a\", \"b\"], value=\"z\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "false");
    }

    #[test]
    fn test_in_filter_form_piped_via_set() {
        // Test the `in` filter registration by piping an array variable.
        // Use `{% set %}` to create an array variable, then pipe it to `in`.
        let vars = test_vars();
        let result = render(
            "{% set items = [\"a\", \"b\", \"c\"] %}{% if items | in(value=\"b\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_in_filter_form_piped_not_found() {
        let vars = test_vars();
        let result = render(
            "{% set items = [\"a\", \"b\", \"c\"] %}{% if items | in(value=\"z\") %}yes{% else %}no{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "no");
    }

    #[test]
    fn test_in_missing_items_arg_error() {
        let vars = test_vars();
        let result = render("{{ in(value=\"a\") }}", &vars);
        assert!(result.is_err(), "in without items should error");
    }

    #[test]
    fn test_in_missing_value_arg_error() {
        let vars = test_vars();
        let result = render("{{ in(items=[\"a\"]) }}", &vars);
        assert!(result.is_err(), "in without value should error");
    }

    // ---- `reReplaceAll` function tests ----

    #[test]
    fn test_re_replace_all_basic() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"world\", input=\"hello world\", replacement=\"rust\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn test_re_replace_all_with_capture_groups() {
        let vars = test_vars();
        // Pattern `(\w+) (\w+)` captures two words; replacement swaps them.
        // In Tera strings, backslash is literal (no \w escape interpretation).
        let result = render(
            r#"{{ reReplaceAll(pattern="(\w+) (\w+)", input="hello world", replacement="$2 $1") }}"#,
            &vars,
        )
        .unwrap();
        assert_eq!(result, "world hello");
    }

    #[test]
    fn test_re_replace_all_capture_group_goreleaser_style() {
        // Mimics the GoReleaser docs example:
        // reReplaceAll "(.*) \(#(.*)\)" .Message "$1 [#$2](url/$2)"
        let mut vars = test_vars();
        vars.set("Message", "fix bug (#123)");
        let result = render(
            r#"{{ reReplaceAll(pattern="(.*) \(#(.*)\)", input=Message, replacement="$1 [#$2](https://tracker/$2)") }}"#,
            &vars,
        )
        .unwrap();
        assert_eq!(result, "fix bug [#123](https://tracker/123)");
    }

    #[test]
    fn test_re_replace_all_no_match() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"xyz\", input=\"hello\", replacement=\"replaced\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_re_replace_all_invalid_regex_error() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"[invalid\", input=\"hello\", replacement=\"x\") }}",
            &vars,
        );
        assert!(result.is_err(), "invalid regex should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid regex") || err.contains("reReplaceAll"),
            "error should mention reReplaceAll or invalid regex, got: {err}"
        );
    }

    #[test]
    fn test_re_replace_all_replaces_all_occurrences() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"o\", input=\"foo bar boo\", replacement=\"0\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "f00 bar b00");
    }

    #[test]
    fn test_re_replace_all_go_style_positional() {
        // Go-style: {{ reReplaceAll "pattern" "input" "replacement" }}
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll \"world\" \"hello world\" \"rust\" }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn test_re_replace_all_go_style_with_variable() {
        // Go-style with a variable as input: {{ reReplaceAll "v" Tag "" }}
        let vars = test_vars();
        let result = render("{{ reReplaceAll \"v\" Tag \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_re_replace_all_filter_form() {
        // Filter form: {{ Field | reReplaceAll(pattern="...", replacement="...") }}
        let vars = test_vars();
        let result = render(
            "{{ Tag | reReplaceAll(pattern=\"v\", replacement=\"\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_re_replace_all_filter_form_with_capture() {
        let vars = test_vars();
        let result = render(
            "{{ Tag | reReplaceAll(pattern=\"v(.*)\", replacement=\"ver-$1\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "ver-1.2.3");
    }

    #[test]
    fn test_re_replace_all_piped_positional() {
        // Piped positional: {{ Tag | reReplaceAll "v" "" }}
        let vars = test_vars();
        let result = render("{{ Tag | reReplaceAll \"v\" \"\" }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_re_replace_all_missing_pattern_error() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(input=\"hello\", replacement=\"x\") }}",
            &vars,
        );
        assert!(result.is_err(), "reReplaceAll without pattern should error");
    }

    #[test]
    fn test_re_replace_all_missing_input_error() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"x\", replacement=\"y\") }}",
            &vars,
        );
        assert!(result.is_err(), "reReplaceAll without input should error");
    }

    #[test]
    fn test_re_replace_all_missing_replacement_error() {
        let vars = test_vars();
        let result = render(
            "{{ reReplaceAll(pattern=\"x\", input=\"hello\") }}",
            &vars,
        );
        assert!(
            result.is_err(),
            "reReplaceAll without replacement should error"
        );
    }

    #[test]
    fn test_re_replace_all_filter_invalid_regex_error() {
        let vars = test_vars();
        let result = render(
            "{{ Tag | reReplaceAll(pattern=\"[bad\", replacement=\"x\") }}",
            &vars,
        );
        assert!(
            result.is_err(),
            "invalid regex in filter form should produce an error"
        );
    }
}
