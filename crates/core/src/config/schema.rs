use super::*;

/// JSON Schema for the [`Config`] document as a canonical `serde_json::Value`,
/// in the JSON Schema draft-07 dialect.
///
/// The published `schema.json`, the `anodizer jsonschema` command, and the
/// config-reference doc generator all read the schema from this one function so
/// the dialect (`definitions` + `#/definitions/` refs) and the byte-form are
/// fixed in a single place. draft-07 is the dialect editors (VS Code, the JSON
/// Schema Store) resolve for `.anodizer.yaml`, so the published schema and the
/// editor integration agree.
///
/// Returns a plain `Value` rather than [`schemars::Schema`] deliberately:
/// serializing a `Schema` re-imposes schemars 1.x's keyword ordering (via its
/// internal `OrderedKeywordWrapper`), which would undo [`canonicalize_schema`].
/// Serializing the `Value` directly preserves the canonical order.
#[must_use]
pub fn config_schema() -> serde_json::Value {
    let schema = schemars::generate::SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Config>();
    let mut value = schema.to_value();
    canonicalize_schema(&mut value);
    value
}

/// JSON Schema keyword serialization order matching schemars 0.8's `SchemaObject`
/// field declaration order (its flattened `Metadata` / `SubschemaValidation` /
/// number / string / array / object validation structs concatenated in struct
/// order). The published `schema.json` is byte-pinned to this order so it stays
/// stable across schemars upgrades (1.x emits a different keyword order, and the
/// workspace builds `serde_json` with `preserve_order` — via `stage-publish` —
/// so insertion order leaks into the file unless re-imposed here). An unlisted
/// keyword sorts after all listed ones, then lexicographically.
const SCHEMA_KEYWORD_ORDER: &[&str] = &[
    "$id",
    "$schema",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "type",
    "format",
    "enum",
    "const",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "items",
    "additionalItems",
    "maxItems",
    "minItems",
    "uniqueItems",
    "contains",
    "maxProperties",
    "minProperties",
    "required",
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "$ref",
    "definitions",
];

/// Schema object keys whose VALUE is a map of name → subschema (not a subschema
/// itself). Their entries are sorted by NAME (schemars 0.8 backed these with a
/// `BTreeMap`); every other keyword's value is a schema whose own keys are
/// ordered by [`SCHEMA_KEYWORD_ORDER`].
const SCHEMA_DEFINITION_MAPS: &[&str] = &["properties", "patternProperties", "definitions"];

/// Re-impose schemars 0.8's deterministic serialization on a draft-07 schema
/// `Value` so the published artifact is byte-stable across schemars versions:
/// recursively (1) order each schema object's keys by [`SCHEMA_KEYWORD_ORDER`],
/// (2) sort definition-map entries (`properties`/`definitions`/…) by name,
/// (3) sort `required` (a set), and (4) normalize every `description` to single
/// spaces within a paragraph while preserving blank-line paragraph breaks.
fn canonicalize_schema(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            if let Some(Value::String(d)) = map.get_mut("description") {
                *d = collapse_description(d);
            }
            if let Some(Value::Array(required)) = map.get_mut("required") {
                required.sort_by(|a, b| match (a.as_str(), b.as_str()) {
                    (Some(x), Some(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            }
            // Recurse, treating each value by its role:
            // - a definition-map value (`properties`/`definitions`/…) is a
            //   name→schema map: sort its entries by name, recurse each schema;
            // - `default`/`enum`/`const`/`examples` hold literal instance DATA,
            //   not schemas — never reorder their keys (they preserve the config
            //   struct's serialization order);
            // - every other value is itself a schema (or array of schemas).
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    k if SCHEMA_DEFINITION_MAPS.contains(&k) => {
                        if let Value::Object(entries) = child {
                            sort_object_by_key(entries);
                            for sub in entries.values_mut() {
                                canonicalize_schema(sub);
                            }
                        }
                    }
                    "default" | "enum" | "const" | "examples" => {}
                    _ => canonicalize_schema(child),
                }
            }
            reorder_object(map, SCHEMA_KEYWORD_ORDER);
        }
        Value::Array(items) => {
            for item in items {
                canonicalize_schema(item);
            }
        }
        _ => {}
    }
}

/// Reorder `map`'s entries so listed keys come first in `order`, then any
/// remaining keys lexicographically. `serde_json`'s `preserve_order` feature is
/// active workspace-wide, so a `Map` serializes in insertion order — rebuilding
/// it in the target order fixes the serialized key order.
fn reorder_object(map: &mut serde_json::Map<String, serde_json::Value>, order: &[&str]) {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort_by(|a, b| {
        let rank = |k: &str| order.iter().position(|o| *o == k).unwrap_or(order.len());
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });
    let mut rebuilt = serde_json::Map::with_capacity(map.len());
    for k in keys {
        if let Some(v) = map.remove(&k) {
            rebuilt.insert(k, v);
        }
    }
    *map = rebuilt;
}

/// Sort an object map's entries by key (rebuilt because `preserve_order` keeps
/// insertion order). Used for definition maps where 0.8 emitted `BTreeMap`-sorted
/// names.
fn sort_object_by_key(map: &mut serde_json::Map<String, serde_json::Value>) {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    let mut rebuilt = serde_json::Map::with_capacity(map.len());
    for k in keys {
        if let Some(v) = map.remove(&k) {
            rebuilt.insert(k, v);
        }
    }
    *map = rebuilt;
}

/// Normalize a schema `description`: collapse each paragraph's internal
/// whitespace (including the rustdoc doc-comment's hard line wraps, which
/// schemars 1.x preserves verbatim) to single spaces, while preserving
/// blank-line paragraph breaks (`\n\n`). Reproduces the single-spaced,
/// paragraph-separated form earlier schemars releases emitted, so the published
/// schema's tooltips render as clean prose in editors.
fn collapse_description(s: &str) -> String {
    s.split("\n\n")
        .map(|para| {
            para.split('\n')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
