//! Pure operations on SchemaStore's `catalog.json`.
//! Reads are string-in so they unit-test without git or network.

use serde_json::{Map, Value};

use crate::schemastore::scan::{
    JsonScan, array_contains_element, find_array_close, find_array_open_after, find_bracket_close,
    find_schemas_array_open, first_element_indent, interior_has_element, line_indent,
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
///
/// SchemaStore's real catalog uniqueness key is `fileMatch`, not `name`: its
/// `validate` CI rejects any two entries that share a `fileMatch` glob. Keying
/// add/update identity on a non-empty `fileMatch` intersection (rather than an
/// exact `name`) is therefore what prevents a case- or title-only name drift
/// from appending a duplicate entry the validator then rejects.
fn filematch_overlaps(existing: &Value, desired_file_match: &[String]) -> bool {
    let theirs = file_match_globs(existing);
    desired_file_match
        .iter()
        .any(|d| theirs.iter().any(|t| t == d))
}

/// Decide add/update/no-op for the desired entry `want` against `catalog_json`.
///
/// Identity is by `fileMatch`-overlap, not `name`: an existing catalog entry is
/// "ours" when its `fileMatch` array shares any glob with `want`'s. This matches
/// SchemaStore's own uniqueness rule (its `validate` CI rejects duplicate
/// `fileMatch` globs) and is robust to a `name` that differs only in case from
/// the merged upstream entry. Comparison of a matched entry against `want` is
/// structural (key order irrelevant).
pub(crate) fn verdict(catalog_json: &str, want: &Value) -> anyhow::Result<Verdict> {
    let cat: Value = serde_json::from_str(catalog_json)?;
    let entries = cat
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    let want_fm = file_match_globs(want);
    match entries.iter().find(|e| filematch_overlaps(e, &want_fm)) {
        None => Ok(Verdict::Add),
        Some(existing) if existing == want => Ok(Verdict::NoOp),
        Some(_) => Ok(Verdict::Update),
    }
}

/// Extract the existing `versions` map of the catalog entry that overlaps
/// `desired_file_match` on `fileMatch`, if present. Returns `None` when no
/// entry overlaps or the matched entry has no `versions`; `Some(Err)` only on
/// malformed catalog JSON.
///
/// Identity is by `fileMatch`-overlap, not `name`, so a versioned-vendor entry
/// whose upstream `name` drifted in case still has its prior versions carried
/// forward — a name-keyed lookup would miss it, drop the map, and rebuild from
/// scratch, silently losing older versioned URLs (which SchemaStore CI then
/// rejects as unresolvable listed files).
pub(crate) fn upstream_versions_by_file_match(
    catalog_json: &str,
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
        .find(|e| filematch_overlaps(e, desired_file_match))?;
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

/// Insert or replace the catalog entry matching `entry` by `fileMatch`-overlap,
/// preserving every other byte of the original file.
///
/// SchemaStore's `catalog.json` is ~1 MB, insertion-ordered, and reformatted
/// by prettier in CI. Reserializing the whole file would reorder entries and
/// produce an unreviewable diff, so this edits only the targeted entry's byte
/// span (replace) or appends before the array's closing `]` (add).
///
/// The match is by `fileMatch`-overlap, not `name`, so an upstream entry whose
/// name drifted in case (e.g. `Anodizer` vs `anodizer`) is replaced in place
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
    if arr.iter().any(|e| filematch_overlaps(e, &want_fm)) {
        let (start, end) = find_entry_span(catalog, &want_fm)?;
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

/// Return the `(start, end)` byte span of the entry object whose `fileMatch`
/// array overlaps `want_fm`. `start` is the index of the object's opening `{`;
/// `end` is the index just past its closing `}`.
///
/// Top-level entry objects inside the array are enumerated by brace-balanced
/// scanning (tracking string/escape state so braces inside string values do
/// not perturb the count); each candidate slice is parsed on its own and its
/// `fileMatch` compared for overlap. The first match wins.
fn find_entry_span(catalog: &str, want_fm: &[String]) -> anyhow::Result<(usize, usize)> {
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
                            && filematch_overlaps(&obj, want_fm)
                        {
                            return Ok((s_idx, end));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("no entry with overlapping `fileMatch` found in `schemas` array")
}
