//! Pure builders/validators for the schemastore publisher: slug, description
//! sanitization, `$schema`/`$id` checks, vendor JSON formatting, catalog-entry
//! construction. No I/O — every fn is unit-testable from a string.

/// Lowercase, trim, and replace runs of non-alphanumeric chars with a single `-`.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
