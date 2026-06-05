//! Pure operations on SchemaStore's `catalog.json`.
//! Reads are string-in so they unit-test without git or network.

use serde_json::Value;

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
