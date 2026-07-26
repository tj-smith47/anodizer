//! Builtins that read state from outside the template context: the
//! `envOrDefault` / `isEnvSet` process-environment placeholders and the
//! `readFile` / `mustReadFile` file readers.

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::JsonRegisterExt;

use crate::path_util::expand_tilde;

pub(super) fn register(tera: &mut tera::Tera) {
    // envOrDefault and isEnvSet are registered as placeholder functions here in
    // BASE_TERA so that Tera's parser recognizes them. They are overridden with
    // context-aware closures in render() before actual rendering occurs.
    // See render() for the real implementations that read from the template
    // context's Env map.
    tera.register_json_function(
        "envOrDefault",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
            Ok(Value::String(value))
        },
    );
    tera.register_json_function(
        "isEnvSet",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("isEnvSet requires `name` argument"))?;
            let is_set = std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false);
            Ok(Value::Bool(is_set))
        },
    );

    // --- File reading functions ---

    // readFile(path="file.txt") — reads file, returns empty string on error.
    // Intentionally returns empty on all errors (not just ENOENT).
    // Whitespace is trimmed from the result.
    tera.register_json_function(
        "readFile",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("readFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(resolved.as_ref()).unwrap_or_default();
            Ok(Value::String(content.trim().to_string()))
        },
    );

    // mustReadFile(path="file.txt") — reads file, errors if file doesn't exist
    // Whitespace is trimmed from the result.
    tera.register_json_function(
        "mustReadFile",
        |args: &HashMap<String, Value>| -> TeraResult<Value> {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::message("mustReadFile requires `path` argument"))?;
            let resolved = expand_tilde(path);
            let content = std::fs::read_to_string(resolved.as_ref())
                .map_err(|e| tera::Error::message(format!("mustReadFile: {}: {}", resolved, e)))?;
            Ok(Value::String(content.trim().to_string()))
        },
    );
}
