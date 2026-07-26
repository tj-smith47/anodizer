//! List and map builtins: construction (`list`, `map`), membership (`in`,
//! `contains_any`), lookup (`index`, `indexOrDefault`), regex line filtering
//! (`filter`, `reverseFilter`), sub-slicing (`slice`), and English list joining
//! (`englishJoin`).

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::JsonRegisterExt;

use super::value_to_string;

pub(super) fn register(tera: &mut tera::Tera) {
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
}
