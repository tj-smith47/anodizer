//! Semver increment builtins: `incmajor` / `incminor` / `incpatch` in both
//! function and filter form, over one shared parse-and-increment helper.

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::{JsonRegisterExt, try_get_value};

enum VersionPart {
    Major,
    Minor,
    Patch,
}

/// Parse and increment a semver version string, returning a tera-friendly
/// error when the input isn't valid semver.
///
/// Version-increment behavior, which calls
/// `semver.MustParse(v)` and surfaces a hard template error on non-semver
/// input. Previously every component was best-effort `unwrap_or(0)`, so
/// `{{ "garbage" | incpatch }}` silently returned `"0.0.1"`.
fn increment_version(v: &str, part: VersionPart) -> Result<String, tera::Error> {
    let stripped = v.strip_prefix('v').unwrap_or(v);
    let parts: Vec<&str> = stripped.splitn(3, '.').collect();
    let invalid = || {
        tera::Error::message(format!(
            "incpatch/incminor/incmajor: '{}' is not a valid semver version (expected MAJOR.MINOR.PATCH)",
            v
        ))
    };
    if parts.len() < 3 {
        return Err(invalid());
    }
    let major: u64 = parts
        .first()
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    let minor: u64 = parts
        .get(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(invalid)?;
    let patch: u64 = parts
        .get(2)
        .and_then(|s| {
            // Handle prerelease suffix: "3-rc.1" → "3"
            s.split('-').next().and_then(|n| n.parse().ok())
        })
        .ok_or_else(invalid)?;
    let prefix = if v.starts_with('v') { "v" } else { "" };
    Ok(match part {
        VersionPart::Major => format!("{}{}.0.0", prefix, major + 1),
        VersionPart::Minor => format!("{}{}.{}.0", prefix, major, minor + 1),
        VersionPart::Patch => format!("{}{}.{}.{}", prefix, major, minor, patch + 1),
    })
}

pub(super) fn register(tera: &mut tera::Tera) {
    // --- Version increment functions ---

    // incpatch("1.2.3") → "1.2.4"
    tera.register_json_function(
        "incpatch",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incpatch requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Patch)?))
        },
    );

    // incminor("1.2.3") → "1.3.0"
    tera.register_json_function(
        "incminor",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incminor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Minor)?))
        },
    );

    // incmajor("1.2.3") → "2.0.0"
    tera.register_json_function(
        "incmajor",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let v = args
                .get("v")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("incmajor requires `v` argument"))?;
            Ok(Value::String(increment_version(v, VersionPart::Major)?))
        },
    );

    // --- Dual registration: existing functions also as filters ---

    // incpatch — filter form: {{ "1.2.3" | incpatch }}
    tera.register_json_filter("incpatch", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incpatch", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Patch)?))
    });

    // incminor — filter form: {{ "1.2.3" | incminor }}
    tera.register_json_filter("incminor", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incminor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Minor)?))
    });

    // incmajor — filter form: {{ "1.2.3" | incmajor }}
    tera.register_json_filter("incmajor", |value: &Value, _: &HashMap<String, Value>| {
        let v = try_get_value!("incmajor", "value", String, value);
        Ok(Value::String(increment_version(&v, VersionPart::Major)?))
    });
}
