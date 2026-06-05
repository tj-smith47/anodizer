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

/// Merge a new version into an existing `versions` map (or start fresh),
/// carrying all prior versions forward.
#[allow(dead_code)]
pub(crate) fn merge_versions(
    prior: Option<&Map<String, Value>>,
    version: &str,
    url: &str,
) -> Map<String, Value> {
    let mut m = prior.cloned().unwrap_or_default();
    m.insert(version.to_string(), Value::String(url.to_string()));
    m
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
    out.push_str("  "); // array closes at the 2-space array indent (see entry_indent note)
    out.push_str(&catalog[close..]);
    Ok(out)
}

/// Add `name` to the `highSchemaVersion` array in `schema-validation.jsonc`,
/// preserving comments and all other bytes. Idempotent: a no-op if `name` is
/// already an element. SchemaStore requires this allowlist entry for any
/// vendored schema using draft-2019-09 / 2020-12.
///
/// The file is JSONC (`//` comments), so it is edited textually rather than
/// reserialized: the array's `[`/`]` span is located with a comment- and
/// string-aware scan, and a new `"<name>"` line is spliced in at the existing
/// element indentation, comma-joining the previous last element.
#[allow(dead_code)]
pub(crate) fn add_high_schema_version(jsonc: &str, name: &str) -> anyhow::Result<String> {
    let open = find_array_open_after(jsonc, "highSchemaVersion")?;
    let close = find_bracket_close(jsonc, open)?;
    let interior = &jsonc[open + 1..close];

    if array_contains_element(interior, name) {
        return Ok(jsonc.to_string());
    }

    // Match the indent of the first existing element; fall back to the array
    // open column + 2 when the array is empty.
    let element_indent = first_element_indent(jsonc, open, interior)
        .unwrap_or_else(|| " ".repeat(array_open_column(jsonc, open) + 2));

    // Comma-join only when a real element precedes the insertion point; an
    // empty `[]` (interior is whitespace/comments only) takes no comma.
    let needs_comma = interior_has_element(interior);

    let before = jsonc[..close].trim_end();
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    quoted.push_str(name);
    quoted.push('"');

    let mut out = String::with_capacity(jsonc.len() + element_indent.len() + quoted.len() + 2);
    out.push_str(before);
    if needs_comma {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&element_indent);
    out.push_str(&quoted);
    out.push('\n');
    // Re-emit the closing `]` at its original column (the bytes between the
    // last element and `]` were trimmed; restore the `]`'s own indent).
    out.push_str(&" ".repeat(array_open_column(jsonc, close)));
    out.push_str(&jsonc[close..]);
    Ok(out)
}

/// True if the array interior contains a string element whose decoded value
/// exactly equals `name`. Element-exact (not substring): scans for `"..."`
/// tokens via `JsonScan` and compares each decoded literal, so `"foo-extra"`
/// does not match `foo`.
fn array_contains_element(interior: &str, name: &str) -> bool {
    let bytes = interior.as_bytes();
    let mut scan = JsonScan::new();
    let mut start: Option<usize> = None;
    let mut was_in_string = false;
    for (i, &b) in bytes.iter().enumerate() {
        scan.step(b);
        if scan.in_string && !was_in_string {
            // String just opened on this `"`; the value begins at i+1.
            start = Some(i + 1);
        } else if !scan.in_string && was_in_string {
            // String just closed on this `"`; the value spans [start, i).
            if let Some(s) = start.take()
                && let Some(decoded) = decode_json_string(&interior[s..i])
                && decoded == name
            {
                return true;
            }
        }
        was_in_string = scan.in_string;
    }
    false
}

/// Decode a JSON string literal body (the bytes *between* the quotes), handling
/// the standard escapes. Returns `None` on a malformed escape — the caller then
/// treats the element as non-matching, which is safe (it triggers an insert).
fn decode_json_string(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = (&mut chars).take(4).collect();
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The column (count of bytes from the start of the line) at byte index `idx`.
/// Used to align the inserted element and re-emit the closing `]` at its
/// original indent.
fn array_open_column(text: &str, idx: usize) -> usize {
    let line_start = text[..idx].rfind('\n').map(|n| n + 1).unwrap_or(0);
    idx - line_start
}

/// Detect the leading-whitespace indent of the first existing array element by
/// finding the first structural `"` in the interior and measuring the
/// whitespace before it on its line. Returns `None` for an empty array.
fn first_element_indent(jsonc: &str, open: usize, interior: &str) -> Option<String> {
    let bytes = interior.as_bytes();
    let mut scan = JsonScan::new();
    for (i, &b) in bytes.iter().enumerate() {
        // `step` returns None for `"`, but flips `in_string` true on a string
        // open; detect that transition to find the first element.
        let before = scan.in_string;
        scan.step(b);
        if scan.in_string && !before {
            // Absolute byte index of this opening quote in `jsonc`.
            let abs = open + 1 + i;
            let line_start = jsonc[..abs].rfind('\n').map(|n| n + 1).unwrap_or(0);
            return Some(jsonc[line_start..abs].to_string());
        }
    }
    None
}

/// True if the array interior holds at least one real element (a string
/// literal), as opposed to being empty or whitespace/comments only.
fn interior_has_element(interior: &str) -> bool {
    let mut scan = JsonScan::new();
    let mut was = false;
    for &b in interior.as_bytes() {
        scan.step(b);
        if scan.in_string && !was {
            return true;
        }
        was = scan.in_string;
    }
    false
}

/// Cursor that walks JSON/JSONC bytes while tracking whether the current
/// position is inside a string literal or a `//` line comment, so that
/// structural `{}[]"` only count when they are *outside* both.
///
/// JSON structural characters are all ASCII, so byte iteration is safe and the
/// recorded indices always land on UTF-8 char boundaries. `catalog.json` has
/// no comments (so comment-skipping is inert there); `schema-validation.jsonc`
/// does, and a `]` inside a `//` comment must not be mistaken for structural.
struct JsonScan {
    in_string: bool,
    escaped: bool,
    in_comment: bool,
    /// The previous byte was a `/` outside a string/comment — a second `/`
    /// now would open a `//` line comment.
    prev_slash: bool,
}

impl JsonScan {
    fn new() -> Self {
        Self {
            in_string: false,
            escaped: false,
            in_comment: false,
            prev_slash: false,
        }
    }

    /// Advance over one byte and report whether it is a *structural* character
    /// (i.e. outside any string literal or `//` comment). Returns `Some(b)` for
    /// a structural `{}[]` and `None` otherwise.
    fn step(&mut self, b: u8) -> Option<u8> {
        if self.in_comment {
            // A `//` line comment runs to the next newline; nothing inside it
            // is structural and `"` does not open a string.
            if b == b'\n' {
                self.in_comment = false;
            }
            return None;
        }
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
        if self.prev_slash {
            self.prev_slash = false;
            if b == b'/' {
                self.in_comment = true;
                return None;
            }
        }
        match b {
            b'"' => {
                self.in_string = true;
                None
            }
            b'/' => {
                self.prev_slash = true;
                None
            }
            b'{' | b'}' | b'[' | b']' => Some(b),
            _ => None,
        }
    }
}

/// Locate the `"<key>"` key and return the byte index of the `[` that opens the
/// array immediately following it. The search for `[` runs through the
/// `JsonScan` state machine, so a `[` inside an intervening string or `//`
/// comment is skipped.
fn find_array_open_after(text: &str, key: &str) -> anyhow::Result<usize> {
    let needle = format!("\"{key}\"");
    let key_at = text
        .find(&needle)
        .ok_or_else(|| anyhow::anyhow!("no `{key}` key found"))?;
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    // Resume scanning just past the key token's closing quote so the key's own
    // quotes do not desync the string-state tracker.
    let resume = key_at + needle.len();
    for (i, &b) in bytes.iter().enumerate().skip(resume) {
        if let Some(b'[') = scan.step(b) {
            return Ok(i);
        }
    }
    anyhow::bail!("`{key}` key is not followed by an array")
}

/// Locate the `"schemas"` key and return the byte index of its opening `[`.
fn find_schemas_array_open(catalog: &str) -> anyhow::Result<usize> {
    find_array_open_after(catalog, "schemas")
}

/// Starting at the `[` at `open_bracket_idx`, return the byte index of the `]`
/// that closes it at depth 0.
///
/// Tracks `[`/`]` nesting depth via `JsonScan`, counting only brackets outside
/// any string literal or `//` comment (a `]` in a `description` value or a
/// JSONC comment must not affect depth). The closing index is the `]` that
/// brings the depth back to zero.
fn find_bracket_close(text: &str, open_bracket_idx: usize) -> anyhow::Result<usize> {
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open_bracket_idx) {
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
    anyhow::bail!("array opened at byte {open_bracket_idx} is not closed")
}

/// Return the byte index of the `]` that closes the `schemas` array.
fn find_array_close(catalog: &str) -> anyhow::Result<usize> {
    let open = find_schemas_array_open(catalog)?;
    find_bracket_close(catalog, open)
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
                        let Some(s_idx) = start.take() else {
                            anyhow::bail!(
                                "internal: object close without matching open in `schemas` array"
                            );
                        };
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
