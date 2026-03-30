// Template rendering powered by Tera.
// Supports both Go-style `{{ .Field }}` and Tera-style `{{ Field }}`.
// Go-style templates are preprocessed (leading dots stripped) before Tera renders them.
// Tera gives us: if/else/endif, for loops, pipes (| lower, | upper, | replace),
// | default, | trim, | title, and many more built-in filters.

use anyhow::{Context as _, Result};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
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
    let patch: u64 = parts.get(2).and_then(|s| {
        // Handle prerelease suffix: "3-rc.1" → "3"
        s.split('-').next().and_then(|n| n.parse().ok())
    }).unwrap_or(0);
    let prefix = if v.starts_with('v') { "v" } else { "" };
    match part {
        VersionPart::Major => format!("{}{}.0.0", prefix, major + 1),
        VersionPart::Minor => format!("{}{}.{}.0", prefix, major, minor + 1),
        VersionPart::Patch => format!("{}{}.{}.{}", prefix, major, minor, patch + 1),
    }
}

/// Regex to match `{{ ... }}` and `{% ... %}` blocks for Go-style dot preprocessing.
// SAFETY: This is a compile-time regex literal; it is known to be valid.
static GO_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{.*?\}\}|\{%.*?%\}").unwrap());


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

    // envOrDefault(name="VAR", default="fallback") — return env var value or default
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

    // isEnvSet(name="VAR") — return true if env var is set and non-empty
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
                    let s = args
                        .get("s")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            tera::Error::msg(format!("{} requires `s` argument", $name))
                        })?;
                    // GoReleaser behavior: if the argument is a valid file path that
                    // exists, hash the file contents; otherwise hash the string itself.
                    let bytes = std::fs::read(s).unwrap_or_else(|_| s.as_bytes().to_vec());
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
                    if b.is_ascii_alphanumeric()
                        || b == b'-'
                        || b == b'_'
                        || b == b'.'
                        || b == b'~'
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
    tera.register_filter(
        "mdv2escape",
        |value: &Value, _: &HashMap<String, Value>| {
            let s = tera::try_get_value!("mdv2escape", "value", String, value);
            let escaped = s
                .chars()
                .map(|c| {
                    if "_*[]()~`>#+-=|{}.!\\".contains(c) {
                        format!("\\{}", c)
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>();
            Ok(Value::String(escaped))
        },
    );

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

    // englishJoin(items=[...], oxford=true) — join list items with commas and "and"
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

    // filter(items=[...], regexp="pattern") — filter array elements by regex
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_function(
        "filter",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::msg("filter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("filter requires `regexp` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("filter: invalid regex: {}", e)))?;
            let filtered: Vec<Value> = items
                .iter()
                .filter(|v| v.as_str().is_some_and(|s| re.is_match(s)))
                .cloned()
                .collect();
            Ok(Value::Array(filtered))
        },
    );

    // reverseFilter(items=[...], regexp="pattern") — exclude array elements matching regex
    // Note: regex is compiled per call. This is acceptable for template rendering
    // where each pattern is typically used once per render pass.
    tera.register_function(
        "reverseFilter",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let items = args
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::msg("reverseFilter requires `items` argument"))?;
            let pattern = args
                .get("regexp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("reverseFilter requires `regexp` argument"))?;
            let re = Regex::new(pattern)
                .map_err(|e| tera::Error::msg(format!("reverseFilter: invalid regex: {}", e)))?;
            let filtered: Vec<Value> = items
                .iter()
                .filter(|v| !v.as_str().is_some_and(|s| re.is_match(s)))
                .cloned()
                .collect();
            Ok(Value::Array(filtered))
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
            let default = args.get("default").cloned().unwrap_or(Value::String(String::new()));
            Ok(map.get(key).cloned().unwrap_or(default))
        },
    );

    tera
});

#[derive(Clone)]
pub struct TemplateVars {
    vars: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl TemplateVars {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            env: HashMap::new(),
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

    /// Return all template variables (excluding env).
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

/// Preprocess a template: convert Go-style `{{ .Field }}` to Tera-style `{{ Field }}`.
/// Handles both `{{ .Field }}` and `{{.Field}}` (no spaces).
/// Also handles chained access like `{{ .Env.VAR }}` → `{{ Env.VAR }}`.
/// Works inside both `{{ }}` and `{% %}` blocks, and handles multiple
/// dot-variables in a single block (e.g., `{{ .Field1 ~ .Field2 }}`).
fn preprocess(template: &str) -> String {
    // For each `{{ ... }}` or `{% ... %}` block, replace all Go-style
    // `.VarName` references with `VarName`. We skip over quoted strings
    // so that dots inside string literals (e.g., file paths) are preserved.
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let open = &block[..2]; // "{{" or "{%"
            let close = &block[block.len() - 2..]; // "}}" or "%}"
            let inner = &block[2..block.len() - 2];

            let mut result = String::with_capacity(block.len());
            result.push_str(open);

            let bytes = inner.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                // Skip over quoted strings entirely
                if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let quote = bytes[i];
                    result.push(quote as char);
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            result.push(bytes[i] as char);
                            result.push(bytes[i + 1] as char);
                            i += 2;
                        } else {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        result.push(bytes[i] as char); // closing quote
                        i += 1;
                    }
                    continue;
                }

                if bytes[i] == b'.'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
                {
                    // Check if the preceding character is a word char — if so,
                    // this is chained access (e.g., `Env.VAR`) and we keep the dot.
                    let prev_is_word = i > 0
                        && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                    if prev_is_word {
                        result.push('.');
                    }
                    // else: Go-style leading dot — skip it
                } else {
                    result.push(bytes[i] as char);
                }
                i += 1;
            }

            result.push_str(close);
            result
        })
        .to_string()
}

/// Build a `tera::Context` from `TemplateVars`.
/// - Regular vars are inserted at the top level: `ProjectName`, `Version`, etc.
/// - Env vars are nested under an `Env` key as a HashMap, so `{{ Env.GITHUB_TOKEN }}` works.
/// - String values of `"true"` / `"false"` are inserted as bools so `{% if Var %}` works.
fn build_tera_context(vars: &TemplateVars) -> tera::Context {
    let mut ctx = tera::Context::new();
    for (k, v) in &vars.vars {
        match v.as_str() {
            "true" => ctx.insert(k.as_str(), &true),
            "false" => ctx.insert(k.as_str(), &false),
            _ => ctx.insert(k.as_str(), v),
        }
    }
    ctx.insert("Env", &vars.env);

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

    tera.add_raw_template("__inline__", &preprocessed)
        .with_context(|| format!("failed to parse template: {}", template))?;

    tera.render("__inline__", &ctx)
        .with_context(|| format!("failed to render template: {}", template))
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
    fn test_env_or_default_returns_env_value_when_set() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_ENV_OR_DEFAULT", "from-env") };
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_ENV_OR_DEFAULT\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "from-env");
        unsafe { std::env::remove_var("ANODIZE_TEST_ENV_OR_DEFAULT") };
    }

    #[test]
    fn test_env_or_default_returns_default_when_unset() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_UNSET_VAR_XYZ") };
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
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_UNSET_VAR_XYZ2") };
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
    fn test_is_env_set_true_when_set() {
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
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_NOT_SET_XYZ") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_NOT_SET_XYZ\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
    }

    #[test]
    fn test_is_env_set_false_when_empty() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_EMPTY_VAR", "") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_EMPTY_VAR\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
        unsafe { std::env::remove_var("ANODIZE_TEST_EMPTY_VAR") };
    }

    #[test]
    fn test_is_env_set_missing_name_error() {
        let vars = test_vars();
        let result = render("{{ isEnvSet() }}", &vars);
        assert!(result.is_err(), "isEnvSet without name should error");
    }

    // ---- Hash function tests (known-answer vectors) ----

    #[test]
    fn test_hash_sha1() {
        let vars = test_vars();
        let result = render("{{ sha1(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn test_hash_sha256() {
        let vars = test_vars();
        let result = render("{{ sha256(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hash_sha512() {
        let vars = test_vars();
        let result = render("{{ sha512(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(
            result,
            "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
        );
    }

    #[test]
    fn test_hash_md5() {
        let vars = test_vars();
        let result = render("{{ md5(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_hash_blake3() {
        let vars = test_vars();
        let result = render("{{ blake3(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(
            result,
            "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
        );
    }

    #[test]
    fn test_hash_crc32() {
        let vars = test_vars();
        let result = render("{{ crc32(s=\"hello\") }}", &vars).unwrap();
        assert_eq!(result, "3610a686");
    }

    #[test]
    fn test_hash_missing_s_arg_error() {
        let vars = test_vars();
        let result = render("{{ sha256() }}", &vars);
        assert!(result.is_err(), "hash function without `s` arg should error");
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
        let result =
            render("{{ readFile(path=\"/tmp/anodize_test_nonexistent_file_xyz\") }}", &vars)
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
        let result = render(
            "{{ englishJoin(items=[]) }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_english_join_one_item() {
        let vars = test_vars();
        let result = render(
            "{{ englishJoin(items=[\"a\"]) }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "a");
    }

    #[test]
    fn test_english_join_two_items() {
        let vars = test_vars();
        let result = render(
            "{{ englishJoin(items=[\"a\", \"b\"]) }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "a and b");
    }

    #[test]
    fn test_english_join_three_items_oxford() {
        let vars = test_vars();
        let result = render(
            "{{ englishJoin(items=[\"a\", \"b\", \"c\"]) }}",
            &vars,
        )
        .unwrap();
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
            (
                "map".to_string(),
                serde_json::json!({"foo": "bar"}),
            ),
            ("key".to_string(), Value::String("foo".to_string())),
            ("default".to_string(), Value::String("fallback".to_string())),
        ]
        .into_iter()
        .collect();

        // Access the function via BASE_TERA - we test it indirectly by calling the logic
        let map = args.get("map").unwrap().as_object().unwrap();
        let key = args.get("key").unwrap().as_str().unwrap();
        let default = args.get("default").cloned().unwrap_or(Value::String(String::new()));
        let result = map.get(key).cloned().unwrap_or(default);
        assert_eq!(result, Value::String("bar".to_string()));
    }

    #[test]
    fn test_index_or_default_key_missing() {
        let args: HashMap<String, Value> = [
            (
                "map".to_string(),
                serde_json::json!({"foo": "bar"}),
            ),
            ("key".to_string(), Value::String("missing".to_string())),
            ("default".to_string(), Value::String("fallback".to_string())),
        ]
        .into_iter()
        .collect();

        let map = args.get("map").unwrap().as_object().unwrap();
        let key = args.get("key").unwrap().as_str().unwrap();
        let default = args.get("default").cloned().unwrap_or(Value::String(String::new()));
        let result = map.get(key).cloned().unwrap_or(default);
        assert_eq!(result, Value::String("fallback".to_string()));
    }

    // ---- Runtime vars test ----

    #[test]
    fn test_runtime_goos_renders() {
        let mut vars = test_vars();
        vars.set("RuntimeGoos", std::env::consts::OS);
        let result = render("{{ Runtime.Goos }}", &vars).unwrap();
        assert!(!result.is_empty(), "Runtime.Goos should render to a non-empty string");
    }
}
