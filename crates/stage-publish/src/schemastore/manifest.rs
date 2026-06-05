//! Pure builders/validators for the schemastore publisher: slug, description
//! sanitization, `$schema`/`$id` checks, vendor JSON formatting, catalog-entry
//! construction. No I/O — every fn is unit-testable from a string.

/// Lowercase, trim, and replace runs of non-alphanumeric chars with a single `-`.
pub(crate) fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Reason a description fails SchemaStore's `assertCatalogJsonHasNoBadFields`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DescriptionError {
    Empty,
    ContainsSchemaWord,
    ContainsNewline,
    BadEdge,
}

impl std::fmt::Display for DescriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let m = match self {
            Self::Empty => "description is empty",
            Self::ContainsSchemaWord => "description must not contain the word \"schema\"",
            Self::ContainsNewline => "description must not contain a newline",
            Self::BadEdge => "description must not start or end with , . space tab or -",
        };
        f.write_str(m)
    }
}

impl std::error::Error for DescriptionError {}

/// Validate a catalog `description` against SchemaStore's content rules.
/// Returns the trimmed description on success.
///
/// The empty check uses `trimmed` (whitespace-stripped) so `"   "` becomes
/// `Empty` rather than `BadEdge`. All other checks use `desc` (original) so
/// leading/trailing whitespace is caught by `BadEdge` before the caller
/// receives a value — the two checks target different invariants.
pub(crate) fn sanitize_description(desc: &str) -> Result<String, DescriptionError> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return Err(DescriptionError::Empty);
    }
    if desc.contains('\n') || desc.contains('\r') {
        return Err(DescriptionError::ContainsNewline);
    }
    if desc.to_ascii_lowercase().contains("schema") {
        return Err(DescriptionError::ContainsSchemaWord);
    }
    let bad = [',', '.', ' ', '\t', '-'];
    // Safe: trimmed is non-empty, so desc has at least one non-whitespace
    // char; but we need the ORIGINAL first/last chars (not trimmed) to
    // reject leading/trailing whitespace as BadEdge.
    let first = match desc.chars().next() {
        Some(c) => c,
        None => return Err(DescriptionError::Empty),
    };
    let last = match desc.chars().last() {
        Some(c) => c,
        None => return Err(DescriptionError::Empty),
    };
    if bad.contains(&first) || bad.contains(&last) {
        return Err(DescriptionError::BadEdge);
    }
    Ok(trimmed.to_string())
}

/// Result of classifying a schema's `$schema` against SchemaStore's CI gate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// draft-04/06/07 — accepted unconditionally.
    Ok,
    /// 2019-09 / 2020-12 — rejected unless allowlisted in `highSchemaVersion`.
    TooHigh,
    /// Not a recognized json-schema dialect URL.
    Unknown,
}

/// Classify a `$schema` URL. Mirrors SchemaStore's `SchemaDialects` table.
pub(crate) fn classify_dialect(schema_url: &str) -> Dialect {
    let u = schema_url.trim_end_matches('#');
    if u.contains("/draft-04/") || u.contains("/draft-06/") || u.contains("/draft-07/") {
        Dialect::Ok
    } else if u.contains("/draft/2019-09/") || u.contains("/draft/2020-12/") {
        Dialect::TooHigh
    } else {
        Dialect::Unknown
    }
}

/// Reformat a schema's JSON to SchemaStore's prettier defaults (2-space indent,
/// trailing newline). Preserves key order (serde_json `preserve_order`).
pub(crate) fn format_vendor_schema(raw: &str) -> anyhow::Result<String> {
    let v: serde_json::Value = serde_json::from_str(raw)?;
    let mut s = serde_json::to_string_pretty(&v)?;
    s.push('\n');
    Ok(s)
}

/// SchemaStore requires `$id` to be an absolute http(s) URL.
pub(crate) fn check_id(id: Option<&str>) -> anyhow::Result<()> {
    match id {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => Ok(()),
        Some(s) => anyhow::bail!("schema `$id` must be an http(s) URL, got `{s}`"),
        None => anyhow::bail!("schema is missing a `$id` (SchemaStore requires an http(s) `$id`)"),
    }
}
