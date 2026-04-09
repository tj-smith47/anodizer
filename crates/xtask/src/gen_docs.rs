use std::fs;
use std::path::Path;
use tera::{Context, Tera};

/// Run doc generation. If `check` is true, compare output against existing files
/// and return an error if they differ.
pub fn run(check: bool) -> Result<(), String> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or("cannot find project root")?;

    let docs_dir = project_root.join("docs/site/content/docs");
    let templates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");

    let tera = Tera::new(
        templates_dir
            .join("*.tera")
            .to_str()
            .ok_or("invalid template path")?,
    )
    .map_err(|e| format!("failed to load templates: {e}"))?;

    let cli_content = generate_cli_reference(&tera)?;
    let config_content = generate_config_reference(&tera)?;

    let cli_path = docs_dir.join("reference/cli.md");
    let config_path = docs_dir.join("reference/configuration.md");

    if check {
        let existing_cli = fs::read_to_string(&cli_path)
            .map_err(|e| format!("cannot read {}: {e}", cli_path.display()))?;
        let existing_config = fs::read_to_string(&config_path)
            .map_err(|e| format!("cannot read {}: {e}", config_path.display()))?;

        let mut stale = false;
        if existing_cli != cli_content {
            eprintln!("STALE: {}", cli_path.display());
            // Print first differing lines to aid debugging
            for (i, (a, b)) in existing_cli.lines().zip(cli_content.lines()).enumerate() {
                if a != b {
                    eprintln!("  line {}: committed: {}", i + 1, a);
                    eprintln!("  line {}: generated: {}", i + 1, b);
                    break;
                }
            }
            if existing_cli.lines().count() != cli_content.lines().count() {
                eprintln!(
                    "  line count: committed={}, generated={}",
                    existing_cli.lines().count(),
                    cli_content.lines().count()
                );
            }
            stale = true;
        }
        if existing_config != config_content {
            eprintln!("STALE: {}", config_path.display());
            for (i, (a, b)) in existing_config
                .lines()
                .zip(config_content.lines())
                .enumerate()
            {
                if a != b {
                    eprintln!("  line {}: committed: {}", i + 1, a);
                    eprintln!("  line {}: generated: {}", i + 1, b);
                    break;
                }
            }
            if existing_config.lines().count() != config_content.lines().count() {
                eprintln!(
                    "  line count: committed={}, generated={}",
                    existing_config.lines().count(),
                    config_content.lines().count()
                );
            }
            stale = true;
        }
        if stale {
            return Err("generated docs are out of date — run `cargo xtask gen-docs`".into());
        }
        eprintln!("docs are up to date");
        return Ok(());
    }

    fs::write(&cli_path, &cli_content)
        .map_err(|e| format!("cannot write {}: {e}", cli_path.display()))?;
    eprintln!("wrote {}", cli_path.display());

    fs::write(&config_path, &config_content)
        .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    eprintln!("wrote {}", config_path.display());

    Ok(())
}

#[derive(serde::Serialize)]
struct ArgInfo {
    long: String,
    short: String,
    default: String,
    help: String,
}

#[derive(serde::Serialize)]
struct CmdInfo {
    name: String,
    about: String,
    args: Vec<ArgInfo>,
}

fn generate_cli_reference(tera: &Tera) -> Result<String, String> {
    let cmd = anodize_cli::build_cli();

    let about = cmd.get_about().map(|a| a.to_string()).unwrap_or_default();

    let global_args: Vec<ArgInfo> = cmd
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| {
            if a.get_help().is_none()
                && let Some(long) = a.get_long()
            {
                eprintln!("warning: global flag --{long} has no help text");
            }
            ArgInfo {
                long: a.get_long().map(|l| format!("`--{l}`")).unwrap_or_default(),
                short: a
                    .get_short()
                    .map(|s| format!("`-{s}`"))
                    .unwrap_or_else(|| "\u{2014}".into()),
                default: "\u{2014}".into(),
                help: a.get_help().map(|h| h.to_string()).unwrap_or_default(),
            }
        })
        .collect();

    let commands: Vec<CmdInfo> = cmd
        .get_subcommands()
        .map(|sub| {
            let args = sub
                .get_arguments()
                .filter(|a| !a.is_global_set() && a.get_id() != "help" && a.get_id() != "version")
                .map(|a| {
                    if a.get_help().is_none() {
                        let flag = a.get_long().unwrap_or_else(|| a.get_id().as_str());
                        eprintln!("warning: {}.--{flag} has no help text", sub.get_name());
                    }
                    ArgInfo {
                        long: a
                            .get_long()
                            .map(|l| format!("`--{l}`"))
                            .unwrap_or_else(|| format!("`<{}>`", a.get_id())),
                        short: a
                            .get_short()
                            .map(|s| format!("`-{s}`"))
                            .unwrap_or_else(|| "\u{2014}".into()),
                        default: a
                            .get_default_values()
                            .first()
                            .map(|d| format!("`{}`", d.to_string_lossy()))
                            .unwrap_or_else(|| "\u{2014}".into()),
                        help: a.get_help().map(|h| h.to_string()).unwrap_or_default(),
                    }
                })
                .collect();
            CmdInfo {
                name: sub.get_name().to_string(),
                about: sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
                args,
            }
        })
        .collect();

    let mut ctx = Context::new();
    ctx.insert("about", &about);
    ctx.insert("global_args", &global_args);
    ctx.insert("commands", &commands);

    tera.render("cli.md.tera", &ctx)
        .map_err(|e| format!("failed to render cli.md: {e}"))
}

// ---------------------------------------------------------------------------
// Config reference generation — schema-driven
// ---------------------------------------------------------------------------

use schemars::Map;
use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};

#[derive(serde::Serialize)]
struct ConfigField {
    name: String,
    field_type: String,
    default: String,
    description: String,
}

#[derive(serde::Serialize)]
struct NestedSection {
    name: String,
    description: String,
    fields: Vec<ConfigField>,
}

/// Map an `InstanceType` variant to a human-readable string.
fn format_instance_type(t: &InstanceType) -> String {
    match t {
        InstanceType::String => "string".into(),
        InstanceType::Integer => "integer".into(),
        InstanceType::Number => "number".into(),
        InstanceType::Boolean => "bool".into(),
        InstanceType::Array => "list".into(),
        InstanceType::Object => "map".into(),
        InstanceType::Null => "null".into(),
    }
}

/// Format a `serde_json::Value` default for markdown (em-dash for absent/null,
/// backtick-wrapped otherwise).
fn format_default(val: &Option<serde_json::Value>) -> String {
    match val {
        None => "\u{2014}".into(),
        Some(serde_json::Value::Null) => "\u{2014}".into(),
        Some(serde_json::Value::String(s)) if s.is_empty() => "\u{2014}".into(),
        Some(serde_json::Value::String(s)) => format!("`{s}`"),
        Some(v) => format!("`{v}`"),
    }
}

/// Given a schema object that may be a `$ref`, extract the definition name
/// (i.e. the fragment after `#/definitions/`).
fn ref_name(reference: &str) -> Option<String> {
    reference
        .strip_prefix("#/definitions/")
        .map(|s| s.to_string())
}

/// Resolve a schema object to a human-readable type name.
///
/// Handles:
/// - Direct `instance_type`
/// - `$ref`
/// - `anyOf` (Option<T> pattern: `[T, null]`)
/// - `allOf` (#[serde(flatten)] pattern)
/// - Array with items `$ref`
fn resolve_type_name(prop: &SchemaObject) -> String {
    // Direct $ref
    if let Some(ref r) = prop.reference {
        return ref_name(r).unwrap_or_else(|| r.clone());
    }

    // anyOf — Option<T> is represented as anyOf: [T, {type: null}]
    if let Some(ref sub) = prop.subschemas {
        if let Some(ref any_of) = sub.any_of {
            // Collect non-null variants
            let non_null: Vec<&Schema> = any_of
                .iter()
                .filter(|s| {
                    if let Schema::Object(o) = s {
                        if let Some(SingleOrVec::Single(ref t)) = o.instance_type {
                            return **t != InstanceType::Null;
                        }
                        // Keep if it has a $ref or subschemas (not a bare null)
                        return o.reference.is_some() || o.subschemas.is_some();
                    }
                    true
                })
                .collect();

            if non_null.len() == 1
                && let Schema::Object(inner) = non_null[0]
            {
                return resolve_type_name(inner);
            }
            // Multiple non-null variants — join them
            let names: Vec<String> = non_null
                .iter()
                .filter_map(|s| {
                    if let Schema::Object(o) = s {
                        Some(resolve_type_name(o))
                    } else {
                        None
                    }
                })
                .collect();
            if !names.is_empty() {
                return names.join(" | ");
            }
        }

        // allOf — flatten pattern; use first entry's type
        if let Some(ref all_of) = sub.all_of
            && let Some(Schema::Object(first)) = all_of.first()
        {
            return resolve_type_name(first);
        }
    }

    // Array with items
    if let Some(ref arr) = prop.array {
        if let Some(SingleOrVec::Single(ref item_schema)) = arr.items
            && let Schema::Object(ref item_obj) = **item_schema
        {
            let inner = resolve_type_name(item_obj);
            return format!("list of {inner}");
        }
        return "list".into();
    }

    // Direct instance_type
    if let Some(ref t) = prop.instance_type {
        return match t {
            SingleOrVec::Single(it) => format_instance_type(it),
            SingleOrVec::Vec(its) => {
                let non_null: Vec<String> = its
                    .iter()
                    .filter(|it| **it != InstanceType::Null)
                    .map(format_instance_type)
                    .collect();
                if non_null.len() == 1 {
                    non_null
                        .into_iter()
                        .next()
                        .expect("filtered to exactly one non-null type")
                } else {
                    non_null.join(" | ")
                }
            }
        };
    }

    "object".into()
}

/// Given a schema object, if it directly or indirectly references a definition,
/// return that definition name. Handles $ref, anyOf with $ref, and array items $ref.
fn resolve_ref_type_name(obj: &SchemaObject) -> Option<String> {
    // Direct $ref
    if let Some(ref r) = obj.reference {
        return ref_name(r);
    }

    // anyOf — look for non-null $ref variant
    if let Some(ref sub) = obj.subschemas
        && let Some(ref any_of) = sub.any_of
    {
        for s in any_of {
            if let Schema::Object(inner) = s
                && let Some(ref r) = inner.reference
            {
                return ref_name(r);
            }
        }
    }

    // Array with items $ref (e.g. signs, upx)
    if let Some(ref arr) = obj.array
        && let Some(SingleOrVec::Single(ref item_schema)) = arr.items
        && let Schema::Object(ref item_obj) = **item_schema
        && let Some(ref r) = item_obj.reference
    {
        return ref_name(r);
    }

    None
}

/// Extract `ConfigField` entries from a properties map.
fn extract_fields(props: &Map<String, Schema>) -> Vec<ConfigField> {
    props
        .iter()
        .map(|(name, schema)| {
            let obj = match schema {
                Schema::Object(o) => o,
                Schema::Bool(_) => {
                    return ConfigField {
                        name: name.clone(),
                        field_type: "any".into(),
                        default: "\u{2014}".into(),
                        description: String::new(),
                    };
                }
            };

            let description = obj
                .metadata
                .as_ref()
                .and_then(|m| m.description.clone())
                .unwrap_or_default()
                .replace('|', "\\|");

            let default = obj.metadata.as_ref().and_then(|m| m.default.clone());

            let field_type = resolve_type_name(obj);

            ConfigField {
                name: name.clone(),
                field_type,
                default: format_default(&default),
                description,
            }
        })
        .collect()
}

fn generate_config_reference(tera: &Tera) -> Result<String, String> {
    let root_schema = schemars::schema_for!(anodize_core::config::Config);
    let defs = &root_schema.definitions;
    let root = &root_schema.schema;

    let root_props = root
        .object
        .as_ref()
        .map(|o| &o.properties)
        .ok_or("Config schema is not an object schema")?;

    // Build top-level field list
    let top_level_fields = extract_fields(root_props);

    // Build nested sections: for every top-level field that references a
    // definition, expand that definition's properties into a section.
    let mut nested_sections: Vec<NestedSection> = Vec::new();

    for (field_name, schema) in root_props.iter() {
        let obj = match schema {
            Schema::Object(o) => o,
            Schema::Bool(_) => continue,
        };

        let def_name = match resolve_ref_type_name(obj) {
            Some(n) => n,
            None => continue,
        };

        let def_schema = match defs.get(&def_name) {
            Some(Schema::Object(s)) => s,
            _ => continue,
        };

        let def_props = match def_schema.object.as_ref() {
            Some(o) if !o.properties.is_empty() => &o.properties,
            _ => continue,
        };

        let description = def_schema
            .metadata
            .as_ref()
            .and_then(|m| m.description.clone())
            .unwrap_or_default();

        let fields = extract_fields(def_props);

        nested_sections.push(NestedSection {
            name: field_name.clone(),
            description,
            fields,
        });
    }

    let mut ctx = Context::new();
    ctx.insert("top_level_fields", &top_level_fields);
    ctx.insert("nested_sections", &nested_sections);

    tera.render("configuration.md.tera", &ctx)
        .map_err(|e| format!("failed to render configuration.md: {e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_schema_has_all_config_fields() {
        let schema = schemars::schema_for!(anodize_core::config::Config);
        let root = schema.schema;
        let props = root
            .object
            .as_ref()
            .expect("Config should be an object schema");
        let field_names: Vec<&String> = props.properties.keys().collect();

        for expected in &[
            "version",
            "project_name",
            "dist",
            "includes",
            "env_files",
            "defaults",
            "before",
            "after",
            "crates",
            "changelog",
            "signs",
            "binary_signs",
            "docker_signs",
            "upx",
            "snapshot",
            "nightly",
            "announce",
            "report_sizes",
            "env",
            "variables",
            "publishers",
            "tag",
            "git",
            "partial",
            "workspaces",
            "source",
            "sboms",
            "release",
            "notarize",
            "metadata",
        ] {
            assert!(
                field_names.contains(&&expected.to_string()),
                "schema missing top-level field: {expected}"
            );
        }
    }

    #[test]
    fn test_all_config_fields_resolve_to_non_empty_type() {
        use schemars::schema::Schema;
        let schema = schemars::schema_for!(anodize_core::config::Config);
        let root = schema.schema;
        let props = root
            .object
            .as_ref()
            .expect("Config should be an object schema");

        for (name, schema) in &props.properties {
            if let Schema::Object(obj) = schema {
                let type_str = super::resolve_type_name(obj);
                assert!(
                    !type_str.is_empty(),
                    "field `{name}` resolved to an empty type string"
                );
            }
        }
    }
}
