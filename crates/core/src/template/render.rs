use anyhow::{Context as _, Result};
use std::collections::HashMap;
use tera::Value;

use crate::env_source::{EnvSource, ProcessEnvSource};
use crate::template_preprocess::{
    preprocess, protect_shell_param_length, restore_shell_param_length,
};

use super::base_tera::{BASE_TERA, translate_go_time_format};
use super::vars::{ENV_REF_RE, NUMERIC_FIELDS, TemplateVars};

/// Build a `tera::Context` from `TemplateVars`, pre-populating missing env var
/// keys referenced in the template with empty strings.
///
/// An empty string is returned for `{{ .Env.NONEXISTENT }}` rather than
/// erroring. Tera's strict mode would error on a missing map key, so we scan
/// the preprocessed template for `Env.VARNAME` references and ensure every
/// referenced key exists in the env map (defaulting to "").
///
/// Fallback semantics: when an `Env.X` key is not in `TemplateVars::env`,
/// `std::env::var(X)` is consulted before defaulting to `""`. This is
/// intentional — these semantics let a user reference any
/// process env var via `{{ Env.X }}`, and the `StageLogger`-level
/// redaction layer (see `crate::redact`) prevents accidental secret
/// prints in the rendered output. A `*_TOKEN` / `*_KEY` / `*_PASSWORD`
/// value flowing through here is redacted before it reaches stderr.
fn build_tera_context_for_template(
    vars: &TemplateVars,
    preprocessed: &str,
    host_env: &dyn EnvSource,
) -> tera::Context {
    // Discover all Env.VARNAME references in the template.
    let referenced_env_keys: Vec<String> = ENV_REF_RE
        .captures_iter(preprocessed)
        .map(|cap| cap[1].to_string())
        .collect();

    // Build an env map that includes all referenced keys, defaulting missing ones to "".
    let mut env_with_defaults = HashMap::new();
    for key in &referenced_env_keys {
        if !vars.env.contains_key(key.as_str()) {
            // Check the injected host env as fallback before defaulting to "".
            let value = host_env.var(key).unwrap_or_default();
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
    // an empty .Var map is provided by default.
    ctx.insert("Var", &vars.custom_vars);

    // Always insert Outputs (even when empty) so that referencing the
    // `Outputs` namespace does not produce a hard Tera error. Accessing a
    // missing key within the map still requires `| default(value="")`.
    ctx.insert("Outputs", &vars.outputs);

    // Build a nested `Runtime` map for `Runtime.Goos` / `Runtime.Goarch`.
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
/// Custom filters are registered:
/// - `tolower` / `toupper` — aliases for Tera's built-in `lower` / `upper`
/// - `trimprefix(prefix="v")` — strip a prefix from a string
/// - `trimsuffix(suffix=".exe")` — strip a suffix from a string
pub fn render(template: &str, vars: &TemplateVars) -> Result<String> {
    render_with_env(template, vars, &ProcessEnvSource)
}

/// Render `template` against `vars`, routing all env reads (Env.X fallback,
/// `envOrDefault` / `isEnvSet` fallback, SDE-aware `time(...)` /
/// `now_format` filters) through the injected `host_env`.
///
/// Production callers use [`render`] (which wires up [`ProcessEnvSource`]);
/// tests inject a closed `MapEnvSource` so deterministic branches can be
/// exercised without mutating the process env.
pub fn render_with_env(
    template: &str,
    vars: &TemplateVars,
    host_env: &dyn EnvSource,
) -> Result<String> {
    // Shield bash `${#…}` from Tera's `{#` comment-open before parsing, so it
    // reaches the rendered output literally (GoReleaser's Go templates have no
    // such collision); the inverse restore runs on the rendered string below.
    let preprocessed_raw = preprocess(template);
    let preprocessed = protect_shell_param_length(&preprocessed_raw)?;
    let ctx = build_tera_context_for_template(vars, preprocessed.as_ref(), host_env);

    // Clone the base instance (cheap — filters carry over, no templates to clone)
    let mut tera = BASE_TERA.clone();

    // Snapshot host-env entries the closures consult into an Arc<HashMap>
    // so tera's `'static` closure requirement is met without leaking a
    // borrow of `host_env` beyond this stack frame. The snapshot is a one-
    // shot pull from the injected source: `MapEnvSource::vars()` returns
    // the fixture map; `ProcessEnvSource::vars()` returns `std::env::vars()`
    // at call time. Tera renderers run synchronously on the calling thread,
    // so the snapshot is consistent for the lifetime of the call even if
    // a sibling thread mutates process env between renders.
    let host_snapshot: HashMap<String, String> = host_env.vars().into_iter().collect();
    let host_snapshot = std::sync::Arc::new(host_snapshot);

    // Override envOrDefault and isEnvSet with closures that read from the
    // template context's Env map. This ensures .env file vars (loaded into
    // TemplateVars via set_env) are visible, not just process env vars.
    // Falls back to the injected `host_env` for vars that exist there but
    // were not explicitly added to the template context.
    let env_map = std::sync::Arc::new(vars.all_env().clone());
    let env_map_for_default = env_map.clone();
    let host_for_default = host_snapshot.clone();
    tera.register_function(
        "envOrDefault",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            // Check template context Env map first, then fall back to the
            // injected host env.
            let value = env_map_for_default
                .get(name)
                .cloned()
                .or_else(|| host_for_default.get(name).cloned())
                .unwrap_or_else(|| default.to_string());
            Ok(Value::String(value))
        },
    );

    let env_map_for_isset = env_map.clone();
    let host_for_isset = host_snapshot.clone();
    tera.register_function(
        "isEnvSet",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("isEnvSet requires `name` argument"))?;
            // Check template context Env map first, then fall back to the
            // injected host env.
            let is_set = env_map_for_isset
                .get(name)
                .map(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    host_for_isset
                        .get(name)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false)
                });
            Ok(Value::Bool(is_set))
        },
    );

    // Re-register the SDE-aware `time` / `now_format` helpers so they
    // resolve `SOURCE_DATE_EPOCH` against the injected host env, not
    // `std::env::var`. BASE_TERA registers placeholder shapes that read
    // through `ProcessEnvSource` — fine for production, but tests injecting
    // a `MapEnvSource` need the overrides to see their fixture value.
    let host_for_time = host_snapshot.clone();
    tera.register_function(
        "time",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("%Y-%m-%dT%H:%M:%SZ");
            let chrono_fmt = translate_go_time_format(fmt);
            let sde = host_for_time.get("SOURCE_DATE_EPOCH").cloned();
            let now = sde
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
                .unwrap_or_else(chrono::Utc::now);
            Ok(Value::String(now.format(&chrono_fmt).to_string()))
        },
    );

    let host_for_now_format = host_snapshot.clone();
    tera.register_filter(
        "now_format",
        move |_value: &Value, args: &HashMap<String, Value>| {
            let fmt = args
                .get("format")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("now_format requires a `format` argument"))?;
            let chrono_fmt = translate_go_time_format(fmt);
            let sde = host_for_now_format.get("SOURCE_DATE_EPOCH").cloned();
            let now = sde
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|secs| chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0))
                .unwrap_or_else(chrono::Utc::now);
            Ok(Value::String(now.format(&chrono_fmt).to_string()))
        },
    );

    tera.add_raw_template("__inline__", preprocessed.as_ref())
        .with_context(|| format!("failed to parse template: {}", template))?;

    let rendered = tera
        .render("__inline__", &ctx)
        .with_context(|| format!("failed to render template: {}", template))?;
    Ok(restore_shell_param_length(&rendered).into_owned())
}

/// Extract the extension from an artifact filename, including compound
/// extensions like `.tar.gz`, `.tar.xz`, `.tar.zst`, `.tar.bz2`, `.tar.lz4`,
/// `.tar.sz`. Returns the extension with a leading dot (e.g. `.tar.gz`, `.exe`,
/// `.dmg`), or an empty string if there is no extension.
///
/// This is the `.ArtifactExt` behavior.
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
