//! Pure operations on SchemaStore's `catalog.json`.
//! Reads are string-in so they unit-test without git or network.

use serde_json::{Map, Value};

use crate::schemastore::scan::{
    JsonScan, array_contains_element, find_array_close, find_array_open_after, find_brace_close,
    find_bracket_close, find_object_member_span, find_object_open_after, find_schemas_array_open,
    first_element_indent, interior_has_element, line_indent,
};

/// What the publisher should do about one schema entry, given the upstream catalog.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    NoOp,
    Add,
    Update,
}

/// Merge a new version into an existing `versions` map (or start fresh),
/// carrying all prior versions forward.
pub(crate) fn merge_versions(
    prior: Option<&Map<String, Value>>,
    version: &str,
    url: &str,
) -> Map<String, Value> {
    let mut m = prior.cloned().unwrap_or_default();
    m.insert(version.to_string(), Value::String(url.to_string()));
    m
}

/// Extract the `fileMatch` globs from a catalog entry `Value` as owned strings.
/// A missing or non-array `fileMatch` yields an empty list.
fn file_match_globs(entry: &Value) -> Vec<String> {
    entry
        .get("fileMatch")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// True when `existing`'s `fileMatch` array shares at least one glob string
/// with `desired_file_match`.
fn filematch_overlaps(existing: &Value, desired_file_match: &[String]) -> bool {
    let theirs = file_match_globs(existing);
    desired_file_match
        .iter()
        .any(|d| theirs.iter().any(|t| t == d))
}

/// True when `existing`'s `name` equals `desired_name` ignoring ASCII case.
fn name_matches(existing: &Value, desired_name: &str) -> bool {
    existing
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.eq_ignore_ascii_case(desired_name))
}

/// The desired entry's `name`, or `""` when it carries none (which then
/// matches no upstream entry).
fn entry_name(entry: &Value) -> &str {
    entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// True when the upstream catalog entry `existing` IS the registration
/// `desired_name`/`desired_file_match` describes.
///
/// SchemaStore's catalog uniqueness rule is BOTH halves: its `validate` CI
/// rejects two entries sharing a `fileMatch` glob AND two entries sharing a
/// `name`. Identity therefore has to be the union — `fileMatch` overlap alone
/// misses an upstream entry submitted with no `fileMatch` at all (nothing can
/// overlap it), and appending beside such an entry is rejected as a duplicate
/// name; `name` alone misses an entry whose name drifted in case or title.
/// The name half is case-insensitive so a title-case upstream rename is
/// updated in place rather than duplicated.
fn same_entry(existing: &Value, desired_name: &str, desired_file_match: &[String]) -> bool {
    filematch_overlaps(existing, desired_file_match)
        || (!desired_name.is_empty() && name_matches(existing, desired_name))
}

/// Decide add/update/no-op for the desired entry `want` against `catalog_json`.
///
/// An existing catalog entry is "ours" when [`same_entry`] holds — its
/// `fileMatch` overlaps `want`'s or its `name` matches case-insensitively.
/// Comparison of a matched entry against `want` is structural (key order
/// irrelevant).
pub(crate) fn verdict(catalog_json: &str, want: &Value) -> anyhow::Result<Verdict> {
    let cat: Value = serde_json::from_str(catalog_json)?;
    let entries = cat
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    let want_fm = file_match_globs(want);
    let want_name = entry_name(want);
    match entries.iter().find(|e| same_entry(e, want_name, &want_fm)) {
        None => Ok(Verdict::Add),
        Some(existing) if existing == want => Ok(Verdict::NoOp),
        Some(_) => Ok(Verdict::Update),
    }
}

/// Extract the existing `versions` map of the catalog entry that IS this
/// registration ([`same_entry`]), if present. Returns `None` when no entry
/// matches or the matched entry has no `versions`; `Some(Err)` only on
/// malformed catalog JSON.
///
/// The lookup uses the same union identity as [`verdict`], so the carry-forward
/// reads the very entry the splice will rewrite. Missing that entry — because
/// its upstream name drifted in case, or because it carries no `fileMatch` at
/// all — would drop the map and rebuild from scratch, silently losing older
/// versioned URLs (which SchemaStore CI then rejects as unresolvable listed
/// files).
pub(crate) fn upstream_versions_for(
    catalog_json: &str,
    desired_name: &str,
    desired_file_match: &[String],
) -> Option<anyhow::Result<Map<String, Value>>> {
    let cat: Value = match serde_json::from_str(catalog_json) {
        Ok(v) => v,
        Err(e) => return Some(Err(e.into())),
    };
    let entry = cat
        .get("schemas")
        .and_then(Value::as_array)?
        .iter()
        .find(|e| same_entry(e, desired_name, desired_file_match))?;
    let versions = entry.get("versions").and_then(Value::as_object)?;
    Some(Ok(versions.clone()))
}

/// Build a catalog entry object with keys in SchemaStore's prettier order
/// (`name`, `description`, `fileMatch`, `url`, then optional `versions`).
///
/// The crate enables serde_json's `preserve_order`, so the insertion order
/// here is the on-disk serialization order. `versions` is appended only when
/// `Some`.
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

/// Insert or replace the catalog entry matching `entry` by [`same_entry`],
/// preserving every other byte of the original file.
///
/// SchemaStore's `catalog.json` is ~1 MB, insertion-ordered, and reformatted
/// by prettier in CI. Reserializing the whole file would reorder entries and
/// produce an unreviewable diff, so this edits only the targeted entry's byte
/// span (replace) or appends before the array's closing `]` (add).
///
/// The match is the union of `fileMatch`-overlap and case-insensitive `name`,
/// so an upstream entry whose name drifted in case (e.g. `Anodizer` vs
/// `anodizer`) or that carries no `fileMatch` at all is replaced in place
/// rather than appended as a SchemaStore-rejected duplicate.
pub(crate) fn splice_entry(catalog: &str, entry: &Value) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(catalog)?;
    let arr = v
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    // SchemaStore indents the `schemas` array at 2 spaces and each entry
    // object at 4 (prettier, 2-space indent).
    let entry_indent = 4usize;

    let want_fm = file_match_globs(entry);
    let want_name = entry_name(entry);
    if arr.iter().any(|e| same_entry(e, want_name, &want_fm)) {
        let (start, end) = find_entry_span(catalog, want_name, &want_fm)?;
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
pub(crate) fn add_high_schema_version(jsonc: &str, name: &str) -> anyhow::Result<String> {
    let open = find_array_open_after(jsonc, "highSchemaVersion")?;
    let close = find_bracket_close(jsonc, open)?;
    let interior = &jsonc[open + 1..close];

    if array_contains_element(interior, name) {
        return Ok(jsonc.to_string());
    }

    // Anchor indentation off the key line (the line holding the array-open `[`),
    // not the `[`/`]` columns. This stays prettier-correct for the multi-line
    // real file, an empty `[]`, and a single-line `[ "a" ]` (which is rewritten
    // to multi-line): the element sits at key-indent + 2, the `]` at key-indent.
    let key_indent = line_indent(jsonc, open);
    let element_indent =
        first_element_indent(jsonc, open, interior).unwrap_or_else(|| format!("{key_indent}  "));

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
    out.push_str(&key_indent);
    out.push_str(&jsonc[close..]);
    Ok(out)
}

/// The per-file validator `options` block `schema-validation.jsonc` records for
/// the vendored file `filename`, or `None` when the file has no `options`
/// object, no block for that name, or a block that does not parse as plain
/// JSON (a block carrying `//` comments). A `None` from an unparseable block is
/// the conservative answer everywhere it is used: the caller then treats the
/// options as absent and rewrites them.
pub(crate) fn schema_options_block(jsonc: &str, filename: &str) -> Option<Map<String, Value>> {
    let open = find_object_open_after(jsonc, "options").ok()?;
    let close = find_brace_close(jsonc, open).ok()?;
    let (start, end) = find_object_member_span(jsonc, open, close, filename)?;
    // Re-wrap the member in braces so serde parses it as a one-key object; that
    // decodes the key's own escapes instead of scanning for a `:` by hand.
    let obj: Value = serde_json::from_str(&format!("{{{}}}", &jsonc[start..end])).ok()?;
    obj.get(filename).and_then(Value::as_object).cloned()
}

/// Insert or replace the per-file validator `options` block for the vendored
/// file `filename` in `schema-validation.jsonc`, preserving comments and all
/// other bytes.
///
/// SchemaStore's `validate` job fails a schema whose `format` values its ajv
/// does not know unless the file has an `options` block declaring them under
/// `unknownFormat`. The file is JSONC, so — like
/// [`add_high_schema_version`] — the member's byte span is located with a
/// comment- and string-aware scan and rewritten in place rather than the file
/// being reserialized.
pub(crate) fn upsert_schema_options(
    jsonc: &str,
    filename: &str,
    block: &Map<String, Value>,
) -> anyhow::Result<String> {
    let open = find_object_open_after(jsonc, "options")?;
    let close = find_brace_close(jsonc, open)?;
    let interior = &jsonc[open + 1..close];

    // Anchor indentation off the key line, matching `add_high_schema_version`:
    // the member sits at key-indent + 2 and the `}` at key-indent, which stays
    // prettier-correct for a multi-line object, an empty `{}`, and a
    // single-line one.
    let key_indent = line_indent(jsonc, open);
    let member_indent =
        first_element_indent(jsonc, open, interior).unwrap_or_else(|| format!("{key_indent}  "));
    let rendered = render_member(filename, block, member_indent.len())?;

    if let Some((start, end)) = find_object_member_span(jsonc, open, close, filename) {
        let mut out = String::with_capacity(jsonc.len() + rendered.len());
        out.push_str(&jsonc[..start]);
        out.push_str(rendered.trim_start());
        out.push_str(&jsonc[end..]);
        return Ok(out);
    }

    let before = jsonc[..close].trim_end();
    let mut out = String::with_capacity(jsonc.len() + rendered.len() + 2);
    out.push_str(before);
    if interior_has_element(interior) {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&rendered);
    out.push('\n');
    out.push_str(&key_indent);
    out.push_str(&jsonc[close..]);
    Ok(out)
}

/// Render `"<key>": { … }` as a prettier-style object member at the given
/// indentation.
fn render_member(key: &str, block: &Map<String, Value>, indent: usize) -> anyhow::Result<String> {
    let body = render_entry(&Value::Object(block.clone()), indent)?;
    let quoted = serde_json::to_string(&Value::String(key.to_string()))?;
    let pad = " ".repeat(indent);
    Ok(format!("{pad}{quoted}: {}", body.trim_start()))
}

/// Return the `(start, end)` byte span of the entry object that IS this
/// registration ([`same_entry`]). `start` is the index of the object's opening
/// `{`; `end` is the index just past its closing `}`.
///
/// Top-level entry objects inside the array are enumerated by brace-balanced
/// scanning (tracking string/escape state so braces inside string values do
/// not perturb the count); each candidate slice is parsed on its own and
/// tested for identity. The first match wins.
fn find_entry_span(
    catalog: &str,
    want_name: &str,
    want_fm: &[String],
) -> anyhow::Result<(usize, usize)> {
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
                            && same_entry(&obj, want_name, want_fm)
                        {
                            return Ok((s_idx, end));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("no entry matching `{want_name}` found in `schemas` array")
}
