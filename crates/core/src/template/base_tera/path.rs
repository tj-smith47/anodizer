//! Path-shaped string builtins: directory and file-name extraction, absolute-path
//! resolution against the current directory, and URL path-segment escaping.

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::{JsonRegisterExt, try_get_value};

pub(super) fn register(tera: &mut tera::Tera) {
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

    // join(elems=[...]) — Go `filepath.Join`: join the non-empty elements and
    // clean the result, so `join "sub" ".." "checksums"` yields `checksums`.
    // Registered as a FUNCTION only: Tera's builtin `join` FILTER is a
    // list-to-string join with different semantics and must keep working.
    tera.register_json_function(
        "join",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let elems = args
                .get("elems")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tera::Error::message("join requires `elems` argument"))?;
            let parts: Vec<String> = elems
                .iter()
                .map(|v| super::value_to_string(v).into_owned())
                .collect();
            Ok(Value::String(filepath_join(&parts)))
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
}

/// Go `path/filepath.Join` semantics with `/` as the separator: drop empty
/// elements, join the rest, then clean the result lexically — collapse `//`,
/// drop `.`, and resolve `..` against the preceding element without touching
/// the filesystem.
///
/// `/` is emitted on every platform, not the host separator Go would use, so a
/// `name_template` that calls `join` produces the same artifact name on every
/// build shard.
fn filepath_join(parts: &[String]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return String::new();
    }
    let rooted = joined.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&last) if last != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    let body = out.join("/");
    match (rooted, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".to_string(),
        (false, false) => body,
    }
}
