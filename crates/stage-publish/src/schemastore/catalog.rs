//! Pure operations on SchemaStore's `catalog.json`.
//! Reads are string-in so they unit-test without git or network.

use serde_json::{Map, Value};

/// What the publisher should do about one schema entry, given the upstream catalog.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    NoOp,
    Add,
    Update,
}

/// Decide add/update/no-op by matching `name` in `catalog_json` against the
/// desired entry `want`. Comparison is structural (key order irrelevant).
#[allow(dead_code)]
pub(crate) fn verdict(catalog_json: &str, name: &str, want: &Value) -> anyhow::Result<Verdict> {
    let cat: Value = serde_json::from_str(catalog_json)?;
    let entries = cat
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    match entries
        .iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
    {
        None => Ok(Verdict::Add),
        Some(existing) if existing == want => Ok(Verdict::NoOp),
        Some(_) => Ok(Verdict::Update),
    }
}

/// Build a catalog entry object with keys in SchemaStore's prettier order
/// (`name`, `description`, `fileMatch`, `url`, then optional `versions`).
///
/// The crate enables serde_json's `preserve_order`, so the insertion order
/// here is the on-disk serialization order. `versions` is appended only when
/// `Some`.
#[allow(dead_code)]
pub(crate) fn build_entry_json(
    name: &str,
    description: &str,
    file_match: &[String],
    url: &str,
    versions: Option<&Map<String, Value>>,
) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), Value::String(name.into()));
    m.insert("description".into(), Value::String(description.into()));
    m.insert(
        "fileMatch".into(),
        Value::Array(file_match.iter().cloned().map(Value::String).collect()),
    );
    m.insert("url".into(), Value::String(url.into()));
    if let Some(v) = versions {
        m.insert("versions".into(), Value::Object(v.clone()));
    }
    Value::Object(m)
}

/// Render an entry as a prettier-style block at the given indentation (number
/// of leading spaces for the object's opening `{`). Every line of serde_json's
/// pretty output is shifted right by `indent` so the inner keys land at
/// `indent + 2`.
fn render_entry(entry: &Value, indent: usize) -> anyhow::Result<String> {
    let pretty = serde_json::to_string_pretty(entry)?;
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for (i, line) in pretty.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&pad);
        out.push_str(line);
    }
    Ok(out)
}

/// Insert or replace the entry named `name` in `catalog`, preserving every
/// other byte of the original file.
///
/// SchemaStore's `catalog.json` is ~1 MB, insertion-ordered, and reformatted
/// by prettier in CI. Reserializing the whole file would reorder entries and
/// produce an unreviewable diff, so this edits only the targeted entry's byte
/// span (replace) or appends before the array's closing `]` (add).
#[allow(dead_code)]
pub(crate) fn splice_entry(catalog: &str, name: &str, entry: &Value) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(catalog)?;
    let arr = v
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    // SchemaStore indents the `schemas` array at 2 spaces and each entry
    // object at 4 (prettier, 2-space indent).
    let entry_indent = 4usize;

    if arr
        .iter()
        .any(|e| e.get("name").and_then(Value::as_str) == Some(name))
    {
        let (start, end) = find_entry_span(catalog, name)?;
        let rendered = render_entry(entry, entry_indent)?;
        // The span already begins at the object's `{` indentation, so strip
        // the leading pad render_entry added to the first line.
        let rendered = rendered.trim_start();
        let mut out = String::with_capacity(catalog.len());
        out.push_str(&catalog[..start]);
        out.push_str(rendered);
        out.push_str(&catalog[end..]);
        return Ok(out);
    }

    // Append before the array's closing `]`, comma-joining if a prior entry
    // exists.
    let close = find_array_close(catalog)?;
    let before = catalog[..close].trim_end();
    let needs_comma = before.ends_with('}');
    let rendered = render_entry(entry, entry_indent)?;
    let mut out = String::with_capacity(catalog.len() + rendered.len() + 2);
    out.push_str(before);
    if needs_comma {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&rendered);
    out.push('\n');
    out.push_str("  "); // indentation for the closing `]`
    out.push_str(&catalog[close..]);
    Ok(out)
}

/// Cursor that walks JSON bytes while tracking whether the current position is
/// inside a string literal, so that structural `{}[]"` only count when they
/// are *outside* a string.
///
/// JSON structural characters are all ASCII, so byte iteration is safe and the
/// recorded indices always land on UTF-8 char boundaries.
struct JsonScan {
    in_string: bool,
    escaped: bool,
}

impl JsonScan {
    fn new() -> Self {
        Self {
            in_string: false,
            escaped: false,
        }
    }

    /// Advance over one byte and report whether it is a *structural* character
    /// (i.e. outside any string literal). Returns `Some(b)` for a structural
    /// `{}[]` and `None` otherwise.
    fn step(&mut self, b: u8) -> Option<u8> {
        if self.in_string {
            if self.escaped {
                // The previous byte was a backslash; this byte is consumed as
                // the escape payload and cannot end the string.
                self.escaped = false;
            } else if b == b'\\' {
                self.escaped = true;
            } else if b == b'"' {
                self.in_string = false;
            }
            return None;
        }
        match b {
            b'"' => {
                self.in_string = true;
                None
            }
            b'{' | b'}' | b'[' | b']' => Some(b),
            _ => None,
        }
    }
}

/// Locate the `"schemas"` key and return the byte index of its opening `[`.
fn find_schemas_array_open(catalog: &str) -> anyhow::Result<usize> {
    let key = catalog
        .find("\"schemas\"")
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` key"))?;
    let bytes = catalog.as_bytes();
    let open = bytes[key..]
        .iter()
        .position(|&b| b == b'[')
        .map(|off| key + off)
        .ok_or_else(|| anyhow::anyhow!("`schemas` key is not followed by an array"))?;
    Ok(open)
}

/// Return the byte index of the `]` that closes the `schemas` array.
///
/// Scans forward from the array's `[`, tracking `[`/`]` and `{`/`}` nesting
/// depth, but only counting brackets that lie outside a JSON string (a `{` in
/// a `description` value must not affect depth). The closing index is the `]`
/// that brings the array depth back to zero.
fn find_array_close(catalog: &str) -> anyhow::Result<usize> {
    let open = find_schemas_array_open(catalog)?;
    let bytes = catalog.as_bytes();
    let mut scan = JsonScan::new();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if let Some(s) = scan.step(b) {
            match s {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("`schemas` array is not closed")
}

/// Return the `(start, end)` byte span of the entry object whose `"name"`
/// value equals `name`. `start` is the index of the object's opening `{`;
/// `end` is the index just past its closing `}`.
///
/// Top-level entry objects inside the array are enumerated by brace-balanced
/// scanning (tracking string/escape state so braces inside string values do
/// not perturb the count); each candidate slice is parsed on its own and its
/// `name` compared. The first match wins.
fn find_entry_span(catalog: &str, name: &str) -> anyhow::Result<(usize, usize)> {
    let open = find_schemas_array_open(catalog)?;
    let close = find_array_close(catalog)?;
    let bytes = catalog.as_bytes();
    let mut scan = JsonScan::new();
    // Object-nesting depth relative to the array interior; an entry object
    // opens when depth goes 0 -> 1 and closes when it returns 1 -> 0.
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate().take(close).skip(open + 1) {
        if let Some(s) = scan.step(b) {
            match s {
                b'{' => {
                    if depth == 0 {
                        start = Some(i);
                    }
                    depth += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let s_idx = start.take().expect("object close without open");
                        let end = i + 1;
                        if let Ok(obj) = serde_json::from_str::<Value>(&catalog[s_idx..end])
                            && obj.get("name").and_then(Value::as_str) == Some(name)
                        {
                            return Ok((s_idx, end));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("no entry named `{name}` found in `schemas` array")
}
