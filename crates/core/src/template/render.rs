use anyhow::{Context as _, Result};
use std::collections::HashMap;
use tera::Value;

use crate::template_preprocess::preprocess;

use super::base_tera::BASE_TERA;
use super::vars::{ENV_REF_RE, NUMERIC_FIELDS, TemplateVars};

/// Build a `tera::Context` from `TemplateVars`, pre-populating missing env var
/// keys referenced in the template with empty strings.
///
/// GoReleaser returns empty string for `{{ .Env.NONEXISTENT }}` rather than
/// erroring. Tera's strict mode would error on a missing map key, so we scan
/// the preprocessed template for `Env.VARNAME` references and ensure every
/// referenced key exists in the env map (defaulting to "").
///
/// Fallback semantics: when an `Env.X` key is not in `TemplateVars::env`,
/// `std::env::var(X)` is consulted before defaulting to `""`. This is
/// intentional — GoReleaser-compat semantics let a user reference any
/// process env var via `{{ Env.X }}`, and the `StageLogger`-level
/// redaction layer (see `crate::redact`) prevents accidental secret
/// prints in the rendered output. A `*_TOKEN` / `*_KEY` / `*_PASSWORD`
/// value flowing through here is redacted before it reaches stderr.
fn build_tera_context_for_template(vars: &TemplateVars, preprocessed: &str) -> tera::Context {
    // Discover all Env.VARNAME references in the template.
    let referenced_env_keys: Vec<String> = ENV_REF_RE
        .captures_iter(preprocessed)
        .map(|cap| cap[1].to_string())
        .collect();

    // Build an env map that includes all referenced keys, defaulting missing ones to "".
    let mut env_with_defaults = HashMap::new();
    for key in &referenced_env_keys {
        if !vars.env.contains_key(key.as_str()) {
            // Check process env as fallback before defaulting to "".
            let value = std::env::var(key).unwrap_or_default();
            env_with_defaults.insert(key.clone(), value);
        }
    }
    // Overlay with the actual env vars from TemplateVars.
    for (k, v) in &vars.env {
        env_with_defaults.insert(k.clone(), v.clone());
    }

    let mut augmented_vars = vars.clone();
    // Replace the env map with our augmented one.
    augmented_vars.env = env_with_defaults;

    build_tera_context(&augmented_vars)
}

/// Build a `tera::Context` from `TemplateVars`.
/// - Regular vars are inserted at the top level: `ProjectName`, `Version`, etc.
/// - Env vars are nested under an `Env` key as a HashMap, so `{{ Env.GITHUB_TOKEN }}` works.
/// - String values of `"true"` / `"false"` are inserted as bools so `{% if Var %}` works.
/// - Known numeric fields (`Major`, `Minor`, `Patch`, `Timestamp`, `CommitTimestamp`)
///   are inserted as integers so `{% if Major == 1 %}` works correctly.
fn build_tera_context(vars: &TemplateVars) -> tera::Context {
    let mut ctx = tera::Context::new();
    for (k, v) in &vars.vars {
        // For known numeric fields, parse as i64 and insert as a number so
        // Tera comparisons like `{% if Major == 1 %}` work correctly.
        if NUMERIC_FIELDS.contains(&k.as_str())
            && let Ok(n) = v.parse::<i64>()
        {
            ctx.insert(k.as_str(), &n);
            continue;
        }
        match v.as_str() {
            "true" => ctx.insert(k.as_str(), &true),
            "false" => ctx.insert(k.as_str(), &false),
            _ => ctx.insert(k.as_str(), v),
        }
    }
    ctx.insert("Env", &vars.env);

    // Always insert Var (even when empty) so that referencing the `Var`
    // namespace does not produce a hard Tera error. Accessing a missing key
    // within the map still requires `| default(value="")`. This matches
    // GoReleaser which provides an empty .Var map by default.
    ctx.insert("Var", &vars.custom_vars);

    // Always insert Outputs (even when empty) so that referencing the
    // `Outputs` namespace does not produce a hard Tera error. Accessing a
    // missing key within the map still requires `| default(value="")`.
    ctx.insert("Outputs", &vars.outputs);

    // Build a nested `Runtime` map for GoReleaser `Runtime.Goos` / `Runtime.Goarch` compat.
    let mut runtime = HashMap::new();
    if let Some(goos) = vars.vars.get("RuntimeGoos") {
        runtime.insert("Goos".to_string(), goos.clone());
    }
    if let Some(goarch) = vars.vars.get("RuntimeGoarch") {
        runtime.insert("Goarch".to_string(), goarch.clone());
    }
    if !runtime.is_empty() {
        ctx.insert("Runtime", &runtime);
    }

    // Insert structured values (arrays, objects) directly into the context.
    for (k, v) in &vars.structured {
        ctx.insert(k.as_str(), v);
    }

    ctx
}

/// Render a template string with the given variables.
///
/// Supports both Go-style (`{{ .Field }}`) and native Tera-style (`{{ Field }}`).
/// Go-style references are preprocessed into Tera-style before rendering.
///
/// Because this uses Tera under the hood, all Tera features are available:
/// conditionals (`{% if %}` / `{% else %}` / `{% endif %}`), loops (`{% for %}`),
/// filters (`| lower`, `| upper`, `| default`, `| trim`, `| title`, `| replace`, etc.).
///
/// Custom GoReleaser-compat filters are registered:
/// - `tolower` / `toupper` — aliases for Tera's built-in `lower` / `upper`
/// - `trimprefix(prefix="v")` — strip a prefix from a string
/// - `trimsuffix(suffix=".exe")` — strip a suffix from a string
pub fn render(template: &str, vars: &TemplateVars) -> Result<String> {
    let preprocessed = preprocess(template);
    let ctx = build_tera_context_for_template(vars, &preprocessed);

    // Clone the base instance (cheap — filters carry over, no templates to clone)
    let mut tera = BASE_TERA.clone();

    // Override envOrDefault and isEnvSet with closures that read from the
    // template context's Env map. This ensures .env file vars (loaded into
    // TemplateVars via set_env) are visible, not just process env vars.
    // Falls back to std::env::var for vars that exist in the process env
    // but were not explicitly added to the template context.
    let env_map = std::sync::Arc::new(vars.all_env().clone());
    let env_map_for_default = env_map.clone();
    tera.register_function(
        "envOrDefault",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            // Check template context Env map first, then fall back to process env.
            let value = env_map_for_default
                .get(name)
                .cloned()
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_else(|| default.to_string());
            Ok(Value::String(value))
        },
    );

    let env_map_for_isset = env_map.clone();
    tera.register_function(
        "isEnvSet",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("isEnvSet requires `name` argument"))?;
            // Check template context Env map first, then fall back to process env.
            let is_set = env_map_for_isset
                .get(name)
                .map(|v| !v.is_empty())
                .unwrap_or_else(|| std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false));
            Ok(Value::Bool(is_set))
        },
    );

    tera.add_raw_template("__inline__", &preprocessed)
        .with_context(|| format!("failed to parse template: {}", template))?;

    tera.render("__inline__", &ctx)
        .with_context(|| format!("failed to render template: {}", template))
}

/// Extract the extension from an artifact filename, including compound
/// extensions like `.tar.gz`, `.tar.xz`, `.tar.zst`, `.tar.bz2`, `.tar.lz4`,
/// `.tar.sz`. Returns the extension with a leading dot (e.g. `.tar.gz`, `.exe`,
/// `.dmg`), or an empty string if there is no extension.
///
/// This matches GoReleaser's `.ArtifactExt` behavior.
pub fn extract_artifact_ext(filename: &str) -> &str {
    // Check for compound tar extensions first
    const TAR_COMPOUND_SUFFIXES: &[&str] = &[
        ".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tar.lz4", ".tar.sz",
    ];
    let lower = filename.to_ascii_lowercase();
    for suffix in TAR_COMPOUND_SUFFIXES {
        if lower.ends_with(suffix) {
            // Return the slice from the original filename (preserving case)
            return &filename[filename.len() - suffix.len()..];
        }
    }
    // Fall back to the last dot-extension
    match filename.rfind('.') {
        Some(pos) if pos > 0 => &filename[pos..],
        _ => "",
    }
}
