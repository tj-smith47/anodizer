> **STATUS (archived 2026-05-06):** All 47 outlined items delivered.
> xtask deps + 629-line `gen_docs.rs` schema walker, `///` doc comments
> across all 343 fields (carved into 36-file `config/` module), 5 sidebar
> links, 15 new doc pages, parity-session-index jsonschema section.
> SchemaStore.org PR follow-up correctly tracked in `PLAN.md::Task POST-0`
> (gated on live docs URL). Per-checkbox reconciliation skipped.

# Schema-Driven Documentation Generation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the manually-maintained config reference in xtask with auto-generation from the JSON Schema already derived from `config.rs`, add doc comments to all 343 undocumented config fields, update the jsonschema post-release task (already implemented), and add sidebar entries + doc pages for missing commands.

**Architecture:** The `schemars` crate already derives `JsonSchema` on all 102 config types (725 total fields). The xtask currently hardcodes 22 top-level fields and 14 section links — these drift from reality. The fix: add `schemars` + `serde_json` as xtask deps, call `schema_for!(Config)` at gen-docs time, walk the schema tree to extract field name/type/default/description, and render via the existing Tera template. Doc comments (`///`) on config struct fields become the `description` in the schema — the one-time investment of documenting fields pays off for both the doc site and IDE autocompletion via the JSON Schema.

**Tech Stack:** Rust, schemars 0.8, serde_json, Tera, Zola

---

## Task 1: Add `schemars` + `serde_json` to xtask dependencies

**Files:**
- Modify: `crates/xtask/Cargo.toml`

- [ ] **Step 1: Add dependencies**

```toml
# Add these two lines under [dependencies]:
schemars.workspace = true
serde_json.workspace = true
anodizer-core.workspace = true
```

The workspace `Cargo.toml` already has `schemars = "0.8"`. Adding `anodizer-core` directly lets xtask call `schema_for!(Config)` without going through the CLI crate indirection.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p xtask`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/xtask/Cargo.toml
git commit -m "chore(xtask): add schemars, serde_json, anodizer-core deps for schema-driven docs"
```

---

## Task 2: Rewrite `generate_config_reference` to walk the JSON Schema

**Files:**
- Modify: `crates/xtask/src/gen_docs.rs`
- Modify: `crates/xtask/templates/configuration.md.tera`

This is the core change. Replace the hardcoded `top_level_fields` vec and `section_links` vec with schema introspection.

- [ ] **Step 1: Write a test that the schema-derived config reference contains all current top-level fields**

Add to the bottom of `crates/xtask/src/gen_docs.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_has_all_config_fields() {
        let schema = schemars::schema_for!(anodizer_core::config::Config);
        let root = schema.schema;
        let props = root.object.as_ref().expect("Config should be an object schema");
        let field_names: Vec<&String> = props.properties.keys().collect();

        // Every field on the Config struct must appear
        for expected in &[
            "version", "project_name", "dist", "includes", "env_files",
            "defaults", "before", "after", "crates", "changelog", "signs",
            "binary_signs", "docker_signs", "upx", "snapshot", "nightly",
            "announce", "report_sizes", "env", "publishers", "tag",
            "partial", "workspaces", "source", "sbom", "release",
        ] {
            assert!(
                field_names.contains(&&expected.to_string()),
                "schema missing top-level field: {expected}"
            );
        }
    }
}
```

Run: `cargo test -p xtask`
Expected: PASS — confirms schema introspection works from xtask.

- [ ] **Step 2: Add schema walking helpers**

Add these types and functions to `gen_docs.rs`, replacing the existing `ConfigField` and `SectionLink` structs and the `generate_config_reference` function body. Keep `ArgInfo`, `CmdInfo`, and `generate_cli_reference` unchanged.

```rust
use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};
use schemars::Map;

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
    anchor: String,
    description: String,
    fields: Vec<ConfigField>,
}

fn resolve_type_name(prop: &SchemaObject, defs: &Map<String, Schema>) -> String {
    // Direct type
    if let Some(instance) = &prop.instance_type {
        return match instance {
            SingleOrVec::Single(t) => format_instance_type(t),
            SingleOrVec::Vec(types) => types
                .iter()
                .filter(|t| **t != InstanceType::Null)
                .map(format_instance_type)
                .collect::<Vec<_>>()
                .join(" | "),
        };
    }

    // $ref
    if let Some(ref reference) = prop.reference {
        let type_name = reference.rsplit('/').next().unwrap_or(reference);
        return type_name.to_string();
    }

    // anyOf (typically Option<T> becomes anyOf: [T, null])
    if let Some(subschemas) = &prop.subschemas {
        if let Some(any_of) = &subschemas.any_of {
            let non_null: Vec<String> = any_of
                .iter()
                .filter_map(|s| match s {
                    Schema::Object(obj) => {
                        if matches!(&obj.instance_type, Some(SingleOrVec::Single(t)) if **t == InstanceType::Null) {
                            None
                        } else if let Some(ref reference) = obj.reference {
                            Some(reference.rsplit('/').next().unwrap_or(reference).to_string())
                        } else {
                            Some(resolve_type_name(obj, defs))
                        }
                    }
                    _ => None,
                })
                .collect();
            if !non_null.is_empty() {
                return non_null.join(" | ");
            }
        }
        // allOf (produced by #[serde(flatten)] on HashMap fields, e.g. Slack types)
        if let Some(all_of) = &subschemas.all_of {
            for s in all_of {
                if let Schema::Object(obj) = s {
                    if let Some(ref reference) = obj.reference {
                        return reference.rsplit('/').next().unwrap_or(reference).to_string();
                    }
                }
            }
        }
    }

    // Array with items ref
    if let Some(arr) = &prop.array {
        if let Some(Schema::Object(items)) = arr.items.as_ref().and_then(|i| match i {
            SingleOrVec::Single(s) => Some(s.as_ref()),
            _ => None,
        }) {
            let inner = resolve_type_name(items, defs);
            return format!("list of {inner}");
        }
    }

    "object".to_string()
}

fn format_instance_type(t: &InstanceType) -> String {
    match t {
        InstanceType::String => "string",
        InstanceType::Number | InstanceType::Integer => "integer",
        InstanceType::Boolean => "bool",
        InstanceType::Array => "list",
        InstanceType::Object => "map",
        InstanceType::Null => "null",
    }
    .to_string()
}

fn format_default(val: &Option<serde_json::Value>) -> String {
    match val {
        None => "\u{2014}".to_string(),
        Some(serde_json::Value::Null) => "\u{2014}".to_string(),
        Some(serde_json::Value::String(s)) if s.is_empty() => "`\"\"`".to_string(),
        Some(serde_json::Value::String(s)) => format!("`\"{s}\"`"),
        Some(serde_json::Value::Bool(b)) => format!("`{b}`"),
        Some(serde_json::Value::Number(n)) => format!("`{n}`"),
        Some(serde_json::Value::Array(a)) if a.is_empty() => "`[]`".to_string(),
        Some(serde_json::Value::Object(o)) if o.is_empty() => "`{}`".to_string(),
        Some(v) => format!("`{v}`"),
    }
}

fn extract_fields(
    props: &Map<String, Schema>,
    defs: &Map<String, Schema>,
) -> Vec<ConfigField> {
    props
        .iter()
        .map(|(name, schema)| {
            let obj = match schema {
                Schema::Object(o) => o.clone(),
                _ => SchemaObject::default(),
            };
            let desc = obj
                .metadata
                .as_ref()
                .and_then(|m| m.description.clone())
                .unwrap_or_default();
            let default_val = obj.metadata.as_ref().and_then(|m| m.default.clone());
            ConfigField {
                name: name.clone(),
                field_type: resolve_type_name(&obj, defs),
                default: format_default(&default_val),
                description: desc,
            }
        })
        .collect()
}
```

- [ ] **Step 3: Rewrite `generate_config_reference` to use schema introspection**

Replace the function body of `generate_config_reference`:

```rust
fn generate_config_reference(tera: &Tera) -> Result<String, String> {
    let schema = schemars::schema_for!(anodizer_core::config::Config);
    let defs = schema.definitions.clone();
    let root = &schema.schema;

    // Extract top-level fields
    let top_level_fields: Vec<ConfigField> = if let Some(obj) = &root.object {
        extract_fields(&obj.properties, &defs)
    } else {
        Vec::new()
    };

    // Extract nested sections: for each top-level field that references a
    // definition, resolve it and include its fields as a nested section.
    let mut nested_sections: Vec<NestedSection> = Vec::new();
    if let Some(obj) = &root.object {
        for (name, prop_schema) in &obj.properties {
            let type_name = match prop_schema {
                Schema::Object(o) => resolve_ref_type_name(o),
                _ => None,
            };
            if let Some(ref tn) = type_name {
                if let Some(Schema::Object(def)) = defs.get(tn) {
                    if let Some(def_obj) = &def.object {
                        if !def_obj.properties.is_empty() {
                            let desc = def
                                .metadata
                                .as_ref()
                                .and_then(|m| m.description.clone())
                                .unwrap_or_default();
                            nested_sections.push(NestedSection {
                                name: name.clone(),
                                anchor: name.to_lowercase().replace('_', "-"),
                                description: desc,
                                fields: extract_fields(&def_obj.properties, &defs),
                            });
                        }
                    }
                }
            }
        }
    }

    let mut ctx = Context::new();
    ctx.insert("top_level_fields", &top_level_fields);
    ctx.insert("nested_sections", &nested_sections);

    tera.render("configuration.md.tera", &ctx)
        .map_err(|e| format!("failed to render configuration.md: {e}"))
}

/// Given a schema object that may be a $ref, anyOf containing a $ref, or
/// an array whose items reference a definition, return the definition type name.
/// Handles `#[schemars(schema_with)]` overrides like `signs`, `binary_signs`,
/// and `upx` which produce array schemas with items referencing the inner type.
fn resolve_ref_type_name(obj: &SchemaObject) -> Option<String> {
    // Direct $ref
    if let Some(ref reference) = obj.reference {
        return Some(reference.rsplit('/').next().unwrap_or(reference).to_string());
    }
    // anyOf (typically Option<T> → anyOf: [$ref(T), null])
    if let Some(sub) = &obj.subschemas {
        if let Some(any_of) = &sub.any_of {
            for s in any_of {
                if let Schema::Object(inner) = s {
                    if let Some(ref reference) = inner.reference {
                        return Some(
                            reference.rsplit('/').next().unwrap_or(reference).to_string(),
                        );
                    }
                    // Recurse: anyOf entry might itself be an array with items $ref
                    if let Some(found) = resolve_ref_type_name(inner) {
                        return Some(found);
                    }
                }
            }
        }
    }
    // Array with items $ref (e.g. schema_with producing Vec<SignConfig>)
    if let Some(arr) = &obj.array {
        if let Some(SingleOrVec::Single(item_schema)) = &arr.items {
            if let Schema::Object(items_obj) = item_schema.as_ref() {
                if let Some(ref reference) = items_obj.reference {
                    return Some(
                        reference.rsplit('/').next().unwrap_or(reference).to_string(),
                    );
                }
            }
        }
    }
    None
}
```

- [ ] **Step 4: Update the Tera template**

Replace `crates/xtask/templates/configuration.md.tera`:

```
+++
title = "Configuration Reference"
description = "Complete configuration file reference"
weight = 91
template = "docs.html"
+++

<!-- AUTO-GENERATED by `cargo xtask gen-docs` — do not edit manually -->

Anodizer uses `.anodizer.yaml` (or `.anodizer.toml`) in your project root.

## Top-Level Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
{% for field in top_level_fields -%}
| `{{ field.name }}` | {{ field.field_type }} | {{ field.default }} | {{ field.description }} |
{% endfor %}

{% for section in nested_sections %}
## `{{ section.name }}`
{% if section.description %}
{{ section.description }}
{% endif %}

| Field | Type | Default | Description |
|-------|------|---------|-------------|
{% for field in section.fields -%}
| `{{ field.name }}` | {{ field.field_type }} | {{ field.default }} | {{ field.description }} |
{% endfor %}
{% endfor %}
```

- [ ] **Step 5: Run gen-docs and verify output**

Run: `cargo xtask gen-docs`
Expected: writes both `cli.md` and `configuration.md`. The configuration.md should now have the top-level table plus one section per complex config type (ReleaseConfig, ArchiveConfig, etc.) with all their fields.

Manually inspect `docs/site/content/docs/reference/configuration.md` — confirm it has sections for types like `ReleaseConfig`, `BlobConfig`, `AnnounceConfig`, etc. with their fields.

- [ ] **Step 6: Run tests**

Run: `cargo test -p xtask && cargo test --workspace`
Expected: all pass. The `--check` flag now compares against schema-generated output.

- [ ] **Step 7: Commit**

```bash
git add crates/xtask/src/gen_docs.rs crates/xtask/templates/configuration.md.tera docs/site/content/docs/reference/configuration.md docs/site/content/docs/reference/cli.md
git commit -m "feat(xtask): auto-generate config reference from JSON Schema

Replaces the manually-maintained field list with schema introspection
via schemars::schema_for!(Config). Field descriptions come from
/// doc comments on config struct fields. New fields appear
automatically — no more manual xtask updates."
```

---

## Task 3: Add doc comments to all config struct fields missing descriptions

**Files:**
- Modify: `crates/core/src/config.rs`

Currently 343 of 725 fields (47%) have no `///` doc comment, which means they show empty descriptions in both the JSON Schema and the generated docs. This task adds a `///` comment to every field that's missing one.

The doc comments serve double duty: they appear in the generated config reference AND in the JSON Schema that IDEs use for autocompletion. Write them as concise user-facing descriptions — what the field does, not how it's implemented.

**Approach:** Work through config.rs type-by-type. For each type, read the struct definition, understand each field from context (serde attributes, usage in stage crates, GoReleaser equivalent), and add a `///` comment. Group commits by logical area to keep them reviewable.

- [ ] **Step 1: Document top-level `Config` fields (23 missing)**

Every field on the `Config` struct at the top of `config.rs` needs a `///` comment. Reference the existing documented fields (`version`, `env_files`, `binary_signs`) for style. Examples:

```rust
/// Output directory for all build artifacts.
#[serde(default = "default_dist")]
pub dist: PathBuf,

/// Per-crate release configurations.
pub crates: Vec<CrateConfig>,

/// Changelog generation settings.
pub changelog: Option<ChangelogConfig>,
```

Keep descriptions to one line where possible. Don't repeat the type.

Run: `cargo test -p xtask -- test_schema_has_all_config_fields`
Expected: PASS (this doesn't test descriptions, just field presence — serves as a smoke check that config.rs still compiles).

- [ ] **Step 2: Document `CrateConfig` fields (21 missing)**

`CrateConfig` is the per-crate configuration. Every field needs a comment. Examples:

```rust
/// Crate name (must match Cargo.toml package name).
pub name: String,

/// Path to crate directory relative to workspace root.
pub path: Option<PathBuf>,

/// Build configurations for this crate.
pub builds: Option<Vec<BuildConfig>>,
```

- [ ] **Step 3: Document build-related types**

Types: `BuildConfig` (10 missing), `BuildHooksConfig` (2), `BuildIgnore` (2), `Defaults` (5), `DefaultArchiveConfig` (2), `StructuredHook` (4), `HooksConfig` (2).

Examples:

```rust
// BuildConfig
/// Unique identifier for cross-referencing from archive/sign/nfpm configs.
pub id: Option<String>,

/// Output binary name (supports templates).
pub binary: Option<String>,

/// Cargo features to enable.
pub features: Option<Vec<String>>,
```

- [ ] **Step 4: Document archive and checksum types**

Types: `ArchiveConfig` (6 missing), `FormatOverride` (2), `ChecksumConfig` (5).

Examples:

```rust
// ArchiveConfig
/// Archive format: tar.gz, tar.xz, tar.zst, zip, or binary.
pub format: Option<String>,

/// OS-specific format overrides.
pub format_overrides: Option<Vec<FormatOverride>>,

// ChecksumConfig
/// Hash algorithm (sha256, sha512, sha1, sha3-256, blake2b, blake3, md5, etc.).
pub algorithm: Option<String>,
```

- [ ] **Step 5: Document release and changelog types**

Types: `ReleaseConfig` (11 missing), `GitHubConfig` (2), `ChangelogConfig` (5), `ChangelogFilters` (2), `ChangelogGroup` (3).

- [ ] **Step 6: Document sign and docker types**

Types: `SignConfig` (10 missing), `DockerSignConfig` (8), `DockerConfig` (8), `DockerRetryConfig` (1).

- [ ] **Step 7: Document publisher types**

Types: `PublishConfig` (8), `HomebrewConfig` (4), `HomebrewDependency` (1), `ScoopConfig` (2), `ChocolateyConfig` (5), `ChocolateyDependency` (2), `ChocolateyRepoConfig` (2), `WingetConfig` (2), `WingetDependency` (2), `WingetManifestsRepoConfig` (2), `AurConfig` (8), `KrewConfig` (3), `KrewManifestsRepoConfig` (2), `NixConfig` (1), `NixDependency` (1), `BinstallConfig` (4), `VersionSyncConfig` (2).

- [ ] **Step 8: Document announce types**

Types: `AnnounceConfig` (14), `SlackAnnounce` (7), `SlackAttachment` (6), `DiscordAnnounce` (3), `WebhookConfig` (5), `TelegramAnnounce` (4), `TeamsAnnounce` (3), `MattermostAnnounce` (3), `EmailAnnounce` (2), `RedditAnnounce` (3), `TwitterAnnounce` (2), `MastodonAnnounce` (2), `BlueskyAnnounce` (2), `LinkedInAnnounce` (2), `OpenCollectiveAnnounce` (3), `DiscourseAnnounce` (3).

- [ ] **Step 9: Document remaining types**

Types: `PublisherConfig` (6), `TagConfig` (17), `UpxConfig` (7), `UniversalBinaryConfig` (3), `NfpmConfig` (18), `NfpmContent` (4), `NfpmScripts` (4), `NfpmDebTriggers` (6), `FileInfo` (4), `SourceConfig` (4), `SbomConfig` (2), `SnapshotConfig` (1), `WorkspaceConfig` (7), `RepositoryConfig` (2), `TapConfig` (2), `BucketConfig` (2), `PullRequestBaseConfig` (3), `CommitAuthorConfig` (2).

- [ ] **Step 10: Regenerate and verify**

Run: `cargo xtask gen-docs && cargo xtask gen-docs --check`
Expected: docs regenerated, `--check` passes. Inspect `configuration.md` — every field row in every table should now have a non-empty Description column.

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 11: Verify JSON Schema descriptions**

Run: `cargo run --bin anodizer -- jsonschema 2>/dev/null | python3 -c "
import json, sys
schema = json.load(sys.stdin)
defs = schema.get('definitions', {})
missing = 0
for tn, td in defs.items():
    for fn_, fp in td.get('properties', {}).items():
        if not fp.get('description'):
            missing += 1
            print(f'  MISSING: {tn}.{fn_}')
for fn_, fp in schema.get('properties', {}).items():
    if not fp.get('description'):
        missing += 1
        print(f'  MISSING: Config.{fn_}')
print(f'Total missing: {missing}')
"`

Expected: `Total missing: 0`

- [ ] **Step 12: Commit**

```bash
git add crates/core/src/config.rs docs/site/content/docs/reference/configuration.md
git commit -m "docs: add descriptions to all 343 undocumented config fields

Every config struct field now has a /// doc comment that flows
through to both the JSON Schema (IDE autocompletion) and the
auto-generated configuration reference page."
```

---

## Task 4: Add missing sidebar entries for existing doc pages

**Files:**
- Modify: `docs/site/templates/partials/sidebar.html`

This task ONLY adds sidebar entries for pages that already exist as `.md` files but aren't linked. Sidebar entries for new pages created in Task 5 are added in Task 5 alongside the page creation — never reference a page that doesn't exist yet.

The audit found 5 existing doc pages not in the sidebar: `announce/email.md`, `announce/telegram.md`, `announce/teams.md`, `announce/mattermost.md`, and `advanced/troubleshooting.md`.

- [ ] **Step 1: Add announce provider links to sidebar**

In `sidebar.html`, find the Announce section (currently has Discord, Slack, Webhooks) and add entries for the 4 existing-but-unlinked providers. Insert them alphabetically:

```html
  <div class="sidebar-section">
    <div class="sidebar-section-title">Announce</div>
    <a href="{{ get_url(path='@/docs/announce/discord.md') }}" class="sidebar-link{% if current_path == '/docs/announce/discord/' %} active{% endif %}">Discord</a>
    <a href="{{ get_url(path='@/docs/announce/email.md') }}" class="sidebar-link{% if current_path == '/docs/announce/email/' %} active{% endif %}">Email</a>
    <a href="{{ get_url(path='@/docs/announce/mattermost.md') }}" class="sidebar-link{% if current_path == '/docs/announce/mattermost/' %} active{% endif %}">Mattermost</a>
    <a href="{{ get_url(path='@/docs/announce/slack.md') }}" class="sidebar-link{% if current_path == '/docs/announce/slack/' %} active{% endif %}">Slack</a>
    <a href="{{ get_url(path='@/docs/announce/teams.md') }}" class="sidebar-link{% if current_path == '/docs/announce/teams/' %} active{% endif %}">Teams</a>
    <a href="{{ get_url(path='@/docs/announce/telegram.md') }}" class="sidebar-link{% if current_path == '/docs/announce/telegram/' %} active{% endif %}">Telegram</a>
    <a href="{{ get_url(path='@/docs/announce/webhooks.md') }}" class="sidebar-link{% if current_path == '/docs/announce/webhooks/' %} active{% endif %}">Webhooks</a>
  </div>
```

- [ ] **Step 2: Add troubleshooting to Advanced section**

In the Advanced sidebar section, add after Reproducible Builds:

```html
    <a href="{{ get_url(path='@/docs/advanced/troubleshooting.md') }}" class="sidebar-link{% if current_path == '/docs/advanced/troubleshooting/' %} active{% endif %}">Troubleshooting</a>
```

- [ ] **Step 3: Commit**

```bash
git add docs/site/templates/partials/sidebar.html
git commit -m "docs: add sidebar entries for 5 existing but unlinked pages"
```

---

## Task 5: Create doc pages for implemented features with no documentation

**Files:**
- Create: `docs/site/content/docs/advanced/split-merge.md`
- Create: `docs/site/content/docs/publish/nix.md`
- Create: `docs/site/content/docs/publish/blob-storage.md`
- Create: `docs/site/content/docs/packages/snapcraft.md`
- Create: `docs/site/content/docs/packages/dmg.md`
- Create: `docs/site/content/docs/packages/msi.md`
- Create: `docs/site/content/docs/packages/pkg.md`
- Create: `docs/site/content/docs/announce/reddit.md`
- Create: `docs/site/content/docs/announce/twitter.md`
- Create: `docs/site/content/docs/announce/mastodon.md`
- Create: `docs/site/content/docs/announce/bluesky.md`
- Create: `docs/site/content/docs/announce/linkedin.md`
- Create: `docs/site/content/docs/announce/opencollective.md`
- Create: `docs/site/content/docs/announce/discourse.md`
- Create: `docs/site/content/docs/ci/split-merge-ci.md` (narrative guide for `anodizer publish` and `anodizer announce` standalone commands, integrated with the Split & Merge workflow)
- Modify: `docs/site/templates/partials/sidebar.html` (add sidebar entries for all new announce + CI pages)

For each page: read the corresponding stage crate's `lib.rs` and the config struct in `config.rs` to understand the feature. Write real documentation — config fields table, environment variables, example YAML, and behavioral notes. Follow the format of existing complete pages like `docs/site/content/docs/packages/archives.md` or `docs/site/content/docs/announce/email.md`.

Each page needs Zola frontmatter:

```
+++
title = "Page Title"
description = "One-line description"
weight = N
template = "docs.html"
+++
```

- [ ] **Step 1: Create Split & Merge page**

Read `crates/core/src/partial.rs` (or wherever split/merge lives) and the CLI flags in `crates/cli/src/lib.rs` for `--split`, `--merge`, `continue --merge`. Document:
- What split/merge is (distributed CI builds)
- `partial.by` config field ("goos" vs "target")
- `anodizer release --split` workflow
- `anodizer continue --merge` workflow
- GitHub Actions matrix example
- Artifact handoff (dist/ directory, artifacts.json)

Reference: `crates/cli/src/commands/release.rs` for flag handling, `crates/core/src/partial.rs` for the merge logic.

- [ ] **Step 2: Create Blob Storage page**

Read `crates/stage-blob/src/lib.rs` and `BlobConfig` in config.rs. Document:
- S3, GCS, Azure provider config
- Templated directory paths
- S3-compatible backends (MinIO, R2, DO Spaces) via `endpoint` + `s3_force_path_style`
- KMS encryption
- `extra_files`, `ids` filter, `disable` (template string)
- Authentication (env vars per provider)
- Full example YAML

- [ ] **Step 3: Create Nix publisher page**

Read the Nix section in `crates/stage-publish/src/lib.rs` and `NixConfig` in config.rs. Document config fields, generated derivation format, repository config, dependencies.

- [ ] **Step 4: Create packaging pages (Snapcraft, DMG, MSI, PKG)**

For each, read the corresponding `crates/stage-{name}/src/lib.rs` and config struct. Document:
- Config fields table
- Required external tools and platform requirements
- Generated output format
- `ids` filter, `disable`, `mod_timestamp`
- Full example YAML

- [ ] **Step 5: Create announce provider pages (Reddit, Twitter, Mastodon, Bluesky, LinkedIn, OpenCollective, Discourse)**

For each, read `crates/stage-announce/src/{name}.rs` and the corresponding config struct. Document:
- Config fields table
- Required environment variables (tokens, secrets)
- `message_template` / `title_template` with template variables
- Full example YAML

If Discourse is not yet implemented in the codebase (check for `crates/stage-announce/src/discourse.rs`), create the page as a Coming Soon skeleton — but do create the page and sidebar entry so the nav is complete.

- [ ] **Step 6: Create standalone commands guide page**

Create `docs/site/content/docs/ci/split-merge-ci.md` — a narrative CI guide covering the standalone pipeline commands. This goes beyond the auto-generated CLI reference tables (which only show flags) to explain **when and why** to use these commands:

- **`anodizer publish`** — when to use it (running publish independently after a release build), what stages it runs (release + publish + blob), typical CI integration (e.g., a manual approval gate between build and publish)
- **`anodizer announce`** — when to use it (posting announcements after publish completes), standalone usage, how it reads from dist/
- **`anodizer continue --merge`** — how it fits in the split/merge workflow (reference the Split & Merge advanced page for the full workflow)
- Example GitHub Actions workflow showing all three as separate jobs in a pipeline: build (with `--split`) → merge (`continue --merge`) → publish → announce

Reference: `crates/cli/src/commands/` for each command's implementation.

- [ ] **Step 7: Add sidebar entries for all new pages created in this task**

Update `docs/site/templates/partials/sidebar.html`. Every page created in Steps 1-6 needs a sidebar entry. Add them to the sections that already exist from Task 4.

Add to Announce section (alphabetically, merging with entries added in Task 4):
```html
    <a href="{{ get_url(path='@/docs/announce/bluesky.md') }}" class="sidebar-link{% if current_path == '/docs/announce/bluesky/' %} active{% endif %}">Bluesky</a>
    <a href="{{ get_url(path='@/docs/announce/discourse.md') }}" class="sidebar-link{% if current_path == '/docs/announce/discourse/' %} active{% endif %}">Discourse</a>
    <a href="{{ get_url(path='@/docs/announce/linkedin.md') }}" class="sidebar-link{% if current_path == '/docs/announce/linkedin/' %} active{% endif %}">LinkedIn</a>
    <a href="{{ get_url(path='@/docs/announce/mastodon.md') }}" class="sidebar-link{% if current_path == '/docs/announce/mastodon/' %} active{% endif %}">Mastodon</a>
    <a href="{{ get_url(path='@/docs/announce/opencollective.md') }}" class="sidebar-link{% if current_path == '/docs/announce/opencollective/' %} active{% endif %}">OpenCollective</a>
    <a href="{{ get_url(path='@/docs/announce/reddit.md') }}" class="sidebar-link{% if current_path == '/docs/announce/reddit/' %} active{% endif %}">Reddit</a>
    <a href="{{ get_url(path='@/docs/announce/twitter.md') }}" class="sidebar-link{% if current_path == '/docs/announce/twitter/' %} active{% endif %}">Twitter/X</a>
```

Add to Package & Archive section:
```html
    <a href="{{ get_url(path='@/docs/packages/snapcraft.md') }}" class="sidebar-link{% if current_path == '/docs/packages/snapcraft/' %} active{% endif %}">Snapcraft</a>
    <a href="{{ get_url(path='@/docs/packages/dmg.md') }}" class="sidebar-link{% if current_path == '/docs/packages/dmg/' %} active{% endif %}">DMG</a>
    <a href="{{ get_url(path='@/docs/packages/msi.md') }}" class="sidebar-link{% if current_path == '/docs/packages/msi/' %} active{% endif %}">MSI</a>
    <a href="{{ get_url(path='@/docs/packages/pkg.md') }}" class="sidebar-link{% if current_path == '/docs/packages/pkg/' %} active{% endif %}">macOS PKG</a>
```

Add to Publish section:
```html
    <a href="{{ get_url(path='@/docs/publish/nix.md') }}" class="sidebar-link{% if current_path == '/docs/publish/nix/' %} active{% endif %}">Nix</a>
    <a href="{{ get_url(path='@/docs/publish/blob-storage.md') }}" class="sidebar-link{% if current_path == '/docs/publish/blob-storage/' %} active{% endif %}">Blob Storage</a>
```

Add to Advanced section:
```html
    <a href="{{ get_url(path='@/docs/advanced/split-merge.md') }}" class="sidebar-link{% if current_path == '/docs/advanced/split-merge/' %} active{% endif %}">Split &amp; Merge</a>
```

Add to More/CI section:
```html
    <a href="{{ get_url(path='@/docs/ci/split-merge-ci.md') }}" class="sidebar-link{% if current_path == '/docs/ci/split-merge-ci/' %} active{% endif %}">Split &amp; Merge CI</a>
```

- [ ] **Step 8: Commit**

```bash
git add docs/site/content/docs/ docs/site/templates/partials/sidebar.html
git commit -m "docs: add pages for 15 undocumented features

Split/merge, blob storage, Nix publisher, Snapcraft, DMG, MSI,
macOS PKG, Reddit, Twitter, Mastodon, Bluesky, LinkedIn,
OpenCollective, Discourse, standalone commands CI guide."
```

---

## Task 6: Update parity-session-index.md jsonschema post-release task

**Files:**
- Modify: `.claude/specs/parity-session-index.md`

The post-release section lists `anodizer jsonschema` CLI command as a TODO, but it already exists and works (`crates/cli/src/commands/jsonschema.rs`). Update the checklist.

- [ ] **Step 1: Mark jsonschema CLI as done, update remaining items**

In `.claude/specs/parity-session-index.md`, find the "Post-Release: Developer Experience / Infrastructure" section and update:

```markdown
### Post-Release: Developer Experience / Infrastructure

- [x] JSON Schema generation: `anodizer jsonschema` CLI command using schemars-derived schema
- [x] Config reference auto-generated from JSON Schema (xtask gen-docs)
- [ ] Publish JSON Schema to docs site URL
- [ ] Register with SchemaStore.org for auto-discovery (`.anodizer.y{,a}ml` pattern)
- [ ] `# yaml-language-server: $schema=...` inline comment works automatically once schema is published
```

- [ ] **Step 2: Also update PLAN.md if it references this**

Search `PLAN.md` for "jsonschema" or "JSON Schema" references in post-publish sections. If found, mark the CLI command as done there too.

- [ ] **Step 3: Commit**

```bash
git add .claude/specs/parity-session-index.md .claude/PLAN.md
git commit -m "docs: mark jsonschema CLI command as done in parity index and plan"
```

---

## Task 7: Regenerate docs and verify end-to-end

**Files:**
- No new files — verification only

- [ ] **Step 1: Regenerate all docs**

Run: `cargo xtask gen-docs`
Expected: writes cli.md and configuration.md with no warnings.

- [ ] **Step 2: Check freshness**

Run: `cargo xtask gen-docs --check`
Expected: "docs are up to date" — exit 0.

- [ ] **Step 3: Build the Zola site**

Run: `cd docs/site && zola build 2>&1`
Expected: builds with no errors. Check for broken internal link warnings.

- [ ] **Step 4: Run full test suite**

Run: `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: all three pass clean.

- [ ] **Step 5: Spot-check configuration.md**

Open `docs/site/content/docs/reference/configuration.md` and verify:
- Top-level table has all 26 fields (including `partial`, `release`, `blobs` — formerly missing)
- Each nested section has a complete field table with non-empty descriptions
- No `(none)` descriptions remain
- Types are human-readable (not raw schemars JSON)

- [ ] **Step 6: Commit any final fixups**

If the Zola build or spot-check revealed issues, fix and commit.
