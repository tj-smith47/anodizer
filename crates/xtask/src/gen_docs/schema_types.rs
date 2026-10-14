//! The type cell of a configuration reference row: the JSON shape a field's
//! schema names, resolved through `$ref`, `anyOf`, `oneOf` and `allOf`.

use serde_json::{Map, Value};

use super::{SectionAnchors, format_instance_type};

/// The definition name a `#/definitions/<Name>` `$ref` string targets.
fn ref_name(reference: &str) -> Option<String> {
    reference
        .strip_prefix("#/definitions/")
        .map(|s| s.to_string())
}

/// The `$ref` string carried directly on a schema object, if any.
fn direct_ref(schema: &Value) -> Option<&str> {
    schema.as_object()?.get("$ref")?.as_str()
}

/// The non-null members of an `anyOf`/`oneOf` subschema list, treating
/// `{ "type": "null" }` as the null variant schemars emits for an `Option<T>`.
fn non_null_variants(variants: &[Value]) -> Vec<&Value> {
    variants
        .iter()
        .filter(|s| {
            !matches!(
                s.as_object().and_then(|o| o.get("type")),
                Some(Value::String(t)) if t == "null"
            )
        })
        .collect()
}

/// Resolve a property schema to a human-readable type name, matching the config
/// reference's vocabulary.
///
/// The cell names JSON shapes only: a reader of the
/// reference writes YAML and has no use for `StringOrBool` or `HooksConfig`.
///
/// Handles:
/// - direct `$ref` → the shape of the definition it names: a struct is
///   `object` (linked to its section when `anchors` has one for `section`), a
///   string enum is `string`, a union definition such as `StringOrBool` is
///   its members, and a scalar newtype is the scalar;
/// - a `const` or an `enum` of scalars → its values as literals (`false`,
///   `0 | 1`, `"v1" | "v2"`);
/// - `anyOf`/`oneOf` (the `Option<T>` pattern: `[T, {"type":"null"}]`) → the
///   single non-null variant's type, or a `T | U` join for a true union, with
///   any `list of …` member last so it cannot read as a list of the union;
/// - `allOf` (the `#[serde(default)]`-wrapper pattern) → the first entry's type;
/// - array → `list of <inner>` (or bare `list` when the element type is
///   inexpressible);
/// - a `type` keyword (string or `[T, "null"]` union) → the mapped scalar name.
pub(super) fn resolve_type_name(
    prop: &Value,
    defs: &Map<String, Value>,
    anchors: &SectionAnchors,
    section: &str,
) -> String {
    let Some(obj) = prop.as_object() else {
        return "object".into();
    };

    if let Some(r) = direct_ref(prop) {
        let Some(name) = ref_name(r) else {
            return r.to_string();
        };
        let Some(def) = defs.get(&name) else {
            return "object".into();
        };
        if def.get("properties").is_some() {
            return match anchors.for_field(&name, section) {
                Some(anchor) => format!("[object](#{anchor})"),
                None => "object".into(),
            };
        }
        return resolve_type_name(def, defs, anchors, section);
    }

    // A `const` or an `enum` of scalars lists every value the field takes
    // (`archives: false`, an integer that is `0` or `1`), so the cell shows
    // those values.
    let values: Option<Vec<&Value>> = match (obj.get("const"), obj.get("enum")) {
        (Some(c), _) => Some(vec![c]),
        (None, Some(Value::Array(e))) if !e.is_empty() => Some(e.iter().collect()),
        _ => None,
    };
    if let Some(literals) = values.and_then(|v| {
        v.into_iter()
            .map(scalar_literal)
            .collect::<Option<Vec<_>>>()
    }) {
        return literals.join(" | ");
    }

    // An `anyOf` is how an `Option<T>`/nullable field renders
    // (`[T, {"type":"null"}]`), so the field shows `T`; a union of several
    // types (`anyOf` or `oneOf`, such as the bool-or-template fields) shows
    // each distinct member once, sorted so the same union reads the same
    // wherever it is declared.
    if let Some(Value::Array(variants)) = obj.get("anyOf").or_else(|| obj.get("oneOf")) {
        let non_null = non_null_variants(variants);
        if let [single] = non_null.as_slice() {
            return resolve_type_name(single, defs, anchors, section);
        }
        let mut names: Vec<String> = non_null
            .iter()
            .map(|s| resolve_type_name(s, defs, anchors, section))
            .collect();
        names.sort_by(|a, b| {
            (a.starts_with("list"), a.as_str()).cmp(&(b.starts_with("list"), b.as_str()))
        });
        names.dedup();
        if !names.is_empty() {
            return names.join(" | ");
        }
    }

    if let Some(Value::Array(all_of)) = obj.get("allOf")
        && let Some(first) = all_of.first()
    {
        return resolve_type_name(first, defs, anchors, section);
    }

    // A struct written inline has no definition to link to.
    if obj.get("properties").is_some() {
        return "object".into();
    }

    // `Option<Vec<T>>` renders as `type: ["array", "null"]`, while `Vec<T>`
    // renders as the bare `type: "array"`; both must resolve their `items` type
    // so the field shows `list of <inner>`.
    if schema_has_type(prop, "array") {
        if let Some(item) = array_item(prop) {
            let inner = resolve_type_name(item, defs, anchors, section);
            // A union item type is grouped so `list of (object | string)` does
            // not read as a list of objects or a bare string.
            if inner.contains(" | ") {
                return format!("list of ({inner})");
            }
            return format!("list of {inner}");
        }
        return "list".into();
    }

    match obj.get("type") {
        Some(Value::String(t)) => format_instance_type(t),
        Some(Value::Array(types)) => {
            let non_null: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "null")
                .map(format_instance_type)
                .collect();
            if non_null.is_empty() {
                "object".into()
            } else {
                non_null.join(" | ")
            }
        }
        _ => "object".into(),
    }
}

/// A JSON scalar as the reference writes it: `false`, `3`, `"v2"`.
fn scalar_literal(v: &Value) -> Option<String> {
    match v {
        Value::Bool(_) | Value::Number(_) => Some(v.to_string()),
        Value::String(s) => Some(format!("\"{s}\"")),
        _ => None,
    }
}

/// Whether a schema's `type` keyword names `ty`, accepting both the single form
/// (`"type": "array"`) and the nullable union form (`"type": ["array", "null"]`)
/// schemars emits for an `Option<…>`.
fn schema_has_type(schema: &Value, ty: &str) -> bool {
    match schema.as_object().and_then(|o| o.get("type")) {
        Some(Value::String(t)) => t == ty,
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some(ty)),
        _ => false,
    }
}

/// The element schema of an array schema's `items`, handling both the single-
/// schema form (`"items": {…}`) and the tuple form (`"items": [{…}, …]`).
fn array_item(schema: &Value) -> Option<&Value> {
    match schema.as_object()?.get("items")? {
        Value::Array(items) => items.first(),
        other => Some(other),
    }
}

/// If a property schema directly or indirectly references a definition, the
/// definition name. Handles a direct `$ref`, a `$ref` inside `anyOf`/`oneOf`/
/// `allOf`, and a `$ref` on array `items`.
///
/// `allOf` coverage matters because schemars emits the
/// `#[serde(default = "...")]` + `#[serde(rename = "use")]` pattern (used by
/// `mcp:` among others) as a wrapper object with a default and a single-entry
/// `allOf: [{ $ref: ... }]`. Without this branch the field shows up in the
/// top-level table but never gets a `## <name>` schema section.
pub(super) fn resolve_ref_type_name(schema: &Value) -> Option<String> {
    let obj = schema.as_object()?;

    if let Some(r) = direct_ref(schema) {
        return ref_name(r);
    }

    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = obj.get(key) {
            // A variant may itself be an array of a `$ref` (`archives`:
            // `false` or a list of archive configs).
            if let Some(name) = variants.iter().find_map(resolve_ref_type_name) {
                return Some(name);
            }
        }
    }

    if schema_has_type(schema, "array")
        && let Some(item) = array_item(schema)
        && let Some(r) = direct_ref(item)
    {
        return ref_name(r);
    }

    None
}
