# SchemaStore Publisher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `schemastore:` publisher that registers/refreshes a tool's JSON Schema(s) on SchemaStore at release time (external catalog-entry mode + vendored-file mode), proven by dogfooding anodizer (external no-op) and cfgd (vendor, 4 schemas).

**Architecture:** Top-level Manager-group publisher modeled on `krew`. A thin `run()` orchestrates fork-sync → idempotency-probe → edit → commit → PR via existing `util/pr.rs`/`util/commit.rs`/`util/git*`; all decision logic lives in pure, unit-tested helpers (`catalog.rs`, `manifest.rs`) so the network/git surface stays thin. Field presence drives mode (`url` ⇒ external, `schema_file` ⇒ vendor); a cascade resolves shared fields (per-schema → block → derived).

**Tech Stack:** Rust, serde, anyhow, `serde_json` (preserve-order via the existing dependency), Tera (gates), `gh`/`git` subprocess (stage-publish is allow-listed per `.claude/rules/module-boundaries.md`).

**Spec:** `.claude/specs/2026-06-05-schemastore-publisher.md` — read it first. The `[CI-fact]`/`[arch-fact]` tags there are the verified constraints each task below enforces.

**Canonical template to read before Task 11+:** `crates/stage-publish/src/krew.rs` + `crates/core/src/config/publishers/krew.rs` (closest existing publisher — Manager group, fork PR, close-PR rollback, `with_published_crate_scope`, `should_skip_publisher_with_if`).

---

## File structure

| File | Responsibility |
|---|---|
| `crates/core/src/config/publishers/schemastore.rs` | `SchemastoreConfig` (block) + `SchemaEntry`; cascade resolvers; mode inference; config-level validation |
| `crates/core/src/config/mod.rs` | add `schemastore: SchemastoreConfig` field to top-level `Config` |
| `crates/core/src/config/publishers/mod.rs` | `pub mod schemastore;` + re-export |
| `crates/stage-publish/src/schemastore/mod.rs` | `SchemastorePublisher` (`simple_publisher!`) + `impl Publisher` (run/rollback/preflight/skips_on_nightly) |
| `crates/stage-publish/src/schemastore/manifest.rs` | pure: slug, description sanitize/validate, `$schema` dialect + `$id` checks, vendor JSON formatting, catalog-entry build |
| `crates/stage-publish/src/schemastore/catalog.rs` | pure: idempotency verdict, surgical splice, `versions` merge, `highSchemaVersion` allowlist edit |
| `crates/stage-publish/src/schemastore/tests.rs` | unit tests for the two pure modules |
| `crates/stage-publish/src/lib.rs` | `pub mod schemastore;` |
| `crates/stage-publish/src/registry.rs` | `is_schemastore_configured` + Manager-position instantiation with `collapse_required` |
| `docs/site/public/schema.json` (+ `static/`) | regenerated from the config struct |
| `docs/site/content/docs/publish/schemastore.md` (+ nav, `before-publish.md`) | user docs |

Separate-repo tasks (gated, own commits): anodizer-action token docs; cfgd `.anodizer.yaml` + docs.

---

## Commit convention

This repo uses `task commit -- -m "..."` (lint-gated; bare `git commit` is sandbox-blocked). Stage with `git add <paths>` FIRST, then `task commit`. Subject: `type: subject` (no `#none`). Each task ends in one commit.

---

## Task 1: `SchemaEntry` + `SchemastoreConfig` structs + deserialization

**Files:**
- Create: `crates/core/src/config/publishers/schemastore.rs`
- Modify: `crates/core/src/config/publishers/mod.rs` (add `pub mod schemastore;` and re-export `SchemastoreConfig`)
- Modify: `crates/core/src/config/mod.rs` (add `pub schemastore: SchemastoreConfig` to `Config`; it derives `Default`, so a `#[serde(default)]` field needs no other wiring)

- [ ] **Step 1: Write the failing test** (in `schemastore.rs` under `#[cfg(test)] mod tests`)

```rust
#[test]
fn deserializes_external_and_vendor_entries() {
    let yaml = r#"
repository: { owner: tj-smith47, name: schemastore }
versioned: false
schemas:
  - name: Anodizer
    file_match: [".anodizer.yaml", ".anodizer.yml"]
    url: "https://tj-smith47.github.io/anodizer/schema.json"
    description: "Anodizer Rust release-automation configuration file"
  - name: cfgd-config
    file_match: ["cfgd.yaml"]
    schema_file: "schemas/cfgd-config.schema.json"
    crate: cfgd
"#;
    let cfg: SchemastoreConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.schemas.len(), 2);
    assert_eq!(cfg.schemas[0].name, "Anodizer");
    assert_eq!(cfg.schemas[0].url.as_deref(), Some("https://tj-smith47.github.io/anodizer/schema.json"));
    assert_eq!(cfg.schemas[1].schema_file.as_deref(), Some("schemas/cfgd-config.schema.json"));
    assert_eq!(cfg.schemas[1].crate_.as_deref(), Some("cfgd"));
}

#[test]
fn rejects_unknown_field() {
    let yaml = "schemas: []\nbogus: 1\n";
    assert!(serde_yaml_ng::from_str::<SchemastoreConfig>(yaml).is_err());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-core schemastore::tests::deserializes -- --nocapture`
Expected: FAIL — `SchemastoreConfig` undefined.

- [ ] **Step 3: Write the structs**

```rust
//! `schemastore:` publisher config — registers a tool's JSON Schema(s) on
//! SchemaStore. Field presence selects the mode: `url` ⇒ external (catalog
//! entry only), `schema_file` ⇒ vendor (file copied into the SchemaStore repo).
//! See `.claude/specs/2026-06-05-schemastore-publisher.md`.

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

use super::{CommitAuthorConfig, RepositoryConfig};
use crate::config::string_or_bool::StringOrBool;

/// Top-level `schemastore:` block. Shared fields here are defaults for every
/// entry in `schemas`; a per-entry field overrides them (cascade).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchemastoreConfig {
    /// Fork of `SchemaStore/schemastore` to push branches to and open the PR from.
    pub repository: Option<RepositoryConfig>,
    /// Commit author for the SchemaStore commit (defaults to git config).
    pub commit_author: Option<CommitAuthorConfig>,
    /// Default for `SchemaEntry::versioned`.
    pub versioned: Option<bool>,
    /// Skip the whole publisher. Alias: `disable`.
    #[serde(alias = "disable")]
    pub skip: Option<StringOrBool>,
    /// Tera condition; when it renders falsy the publisher is skipped.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// The schema entries to register/refresh.
    pub schemas: Vec<SchemaEntry>,
}

/// One schema registration. `url` XOR `schema_file` selects the mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchemaEntry {
    /// Catalog display name (may be Title Case, e.g. `Anodizer`).
    pub name: String,
    /// Vendor filename / url basename. Defaults to `name` slugified. Vendor-only.
    pub slug: Option<String>,
    /// Well-known config filenames this schema validates (folder globs need `**/`).
    pub file_match: Vec<String>,
    /// EXTERNAL mode: the URL you host the schema at.
    pub url: Option<String>,
    /// VENDOR mode: repo-root-relative path to the generated schema file.
    pub schema_file: Option<String>,
    /// Crate whose version a vendored/versioned schema tracks (per-crate workspaces).
    #[serde(rename = "crate")]
    pub crate_: Option<String>,
    /// Catalog description (required at publish time; derived if omitted).
    pub description: Option<String>,
    /// Emit a version-suffixed vendored file + `versions` map. Vendor-only.
    pub versioned: Option<bool>,
    /// Whether a failure here fails the release. Collapsed across `schemas`.
    pub required: Option<bool>,
    /// Per-entry skip. Alias: `disable`.
    #[serde(alias = "disable")]
    pub skip: Option<StringOrBool>,
    /// Per-entry Tera condition.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
```

Then in `crates/core/src/config/publishers/mod.rs` add `pub mod schemastore;` and `pub use schemastore::{SchemaEntry, SchemastoreConfig};`. In `crates/core/src/config/mod.rs`, add to `Config`:

```rust
    /// SchemaStore publisher (top-level; see `publishers::schemastore`).
    pub schemastore: crate::config::publishers::SchemastoreConfig,
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-core schemastore::tests`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config/publishers/schemastore.rs crates/core/src/config/publishers/mod.rs crates/core/src/config/mod.rs
task commit -- -m "feat(config): add schemastore publisher config structs"
```

---

## Task 2: Cascade resolvers

**Files:**
- Modify: `crates/core/src/config/publishers/schemastore.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn per_entry_versioned_overrides_block_default() {
    let cfg = SchemastoreConfig {
        versioned: Some(false),
        schemas: vec![
            SchemaEntry { name: "a".into(), versioned: None, ..Default::default() },
            SchemaEntry { name: "b".into(), versioned: Some(true), ..Default::default() },
        ],
        ..Default::default()
    };
    assert_eq!(cfg.resolved_versioned(&cfg.schemas[0]), false); // inherits block
    assert_eq!(cfg.resolved_versioned(&cfg.schemas[1]), true);  // overrides
}

#[test]
fn repository_and_author_fall_through_to_block() {
    let repo = RepositoryConfig { owner: Some("tj-smith47".into()), name: Some("schemastore".into()), ..Default::default() };
    let cfg = SchemastoreConfig {
        repository: Some(repo),
        schemas: vec![SchemaEntry { name: "a".into(), ..Default::default() }],
        ..Default::default()
    };
    assert_eq!(cfg.resolved_repository(&cfg.schemas[0]).unwrap().owner.as_deref(), Some("tj-smith47"));
}
```

> Note: `SchemaEntry` has no per-entry `repository`/`commit_author` (one fork/author per PR). The resolver returns the block value; the test pins fall-through. If a later need arises, add the per-entry field and prefer it — but YAGNI for now.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-core schemastore::tests::per_entry_versioned`
Expected: FAIL — `resolved_versioned` undefined.

- [ ] **Step 3: Implement resolvers** (in `impl SchemastoreConfig`)

```rust
impl SchemastoreConfig {
    /// Effective `repository` for an entry (block-level; one fork per PR).
    pub fn resolved_repository(&self, _entry: &SchemaEntry) -> Option<&RepositoryConfig> {
        self.repository.as_ref()
    }
    /// Effective `commit_author` (block-level).
    pub fn resolved_commit_author(&self, _entry: &SchemaEntry) -> Option<&CommitAuthorConfig> {
        self.commit_author.as_ref()
    }
    /// Effective `versioned`: per-entry wins, else block default, else false.
    pub fn resolved_versioned(&self, entry: &SchemaEntry) -> bool {
        entry.versioned.or(self.versioned).unwrap_or(false)
    }
    /// Effective `skip`: true if either the entry or the block sets it truthy.
    pub fn resolved_skip(&self, entry: &SchemaEntry) -> bool {
        let block = self.skip.as_ref().map(StringOrBool::as_bool).unwrap_or(false);
        let per = entry.skip.as_ref().map(StringOrBool::as_bool).unwrap_or(false);
        block || per
    }
    /// Effective `if` condition: per-entry wins, else block.
    pub fn resolved_if<'a>(&'a self, entry: &'a SchemaEntry) -> Option<&'a str> {
        entry.if_condition.as_deref().or(self.if_condition.as_deref())
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-core schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config/publishers/schemastore.rs
task commit -- -m "feat(config): schemastore cascade resolvers"
```

---

## Task 3: Mode inference + config validation

**Files:**
- Modify: `crates/core/src/config/publishers/schemastore.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn mode_inferred_from_field_presence() {
    let ext = SchemaEntry { name: "a".into(), url: Some("https://x/s.json".into()), file_match: vec!["a.yaml".into()], ..Default::default() };
    let ven = SchemaEntry { name: "b".into(), schema_file: Some("s.json".into()), file_match: vec!["b.yaml".into()], ..Default::default() };
    assert_eq!(ext.mode().unwrap(), SchemaMode::External);
    assert_eq!(ven.mode().unwrap(), SchemaMode::Vendor);
}

#[test]
fn validate_rejects_neither_both_and_empty_filematch() {
    let neither = SchemaEntry { name: "a".into(), file_match: vec!["a.yaml".into()], ..Default::default() };
    assert!(neither.validate().unwrap_err().to_string().contains("url` or `schema_file"));
    let both = SchemaEntry { name: "a".into(), url: Some("u".into()), schema_file: Some("s".into()), file_match: vec!["a.yaml".into()], ..Default::default() };
    assert!(both.validate().unwrap_err().to_string().contains("not both"));
    let no_fm = SchemaEntry { name: "a".into(), url: Some("u".into()), file_match: vec![], ..Default::default() };
    assert!(no_fm.validate().unwrap_err().to_string().contains("file_match"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-core schemastore::tests::mode_inferred`
Expected: FAIL — `SchemaMode`/`mode`/`validate` undefined.

- [ ] **Step 3: Implement**

```rust
/// Hosting mode, inferred from which source field is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode { External, Vendor }

impl SchemaEntry {
    /// Infer the mode from field presence. Error if neither/both source fields set.
    pub fn mode(&self) -> anyhow::Result<SchemaMode> {
        match (self.url.is_some(), self.schema_file.is_some()) {
            (true, false) => Ok(SchemaMode::External),
            (false, true) => Ok(SchemaMode::Vendor),
            (false, false) => anyhow::bail!("schemastore schema `{}`: set `url` or `schema_file`", self.name),
            (true, true) => anyhow::bail!("schemastore schema `{}`: set `url` or `schema_file`, not both", self.name),
        }
    }

    /// Config-shape validation (mode + file_match). Content rules that need the
    /// resolved description/dialect are checked later in `manifest`.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.mode()?;
        if self.file_match.is_empty() {
            anyhow::bail!("schemastore schema `{}`: `file_match` must list at least one filename", self.name);
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-core schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config/publishers/schemastore.rs
task commit -- -m "feat(config): schemastore mode inference + validation"
```

---

## Task 4: Slug + description sanitization (pure, in `manifest.rs`)

**Files:**
- Create: `crates/stage-publish/src/schemastore/manifest.rs`
- Create: `crates/stage-publish/src/schemastore/mod.rs` (stub: `pub(crate) mod manifest;` + `#[cfg(test)] mod tests;`)
- Create: `crates/stage-publish/src/schemastore/tests.rs` (empty `mod tests {}` to start)
- Modify: `crates/stage-publish/src/lib.rs` (`pub mod schemastore;`)

- [ ] **Step 1: Write the failing test** (in `tests.rs`)

```rust
use crate::schemastore::manifest::{slugify, sanitize_description, DescriptionError};

#[test]
fn slugify_lowercases_and_hyphenates() {
    assert_eq!(slugify("Anodizer"), "anodizer");
    assert_eq!(slugify("My Tool Config"), "my-tool-config");
    assert_eq!(slugify("cfgd-config"), "cfgd-config");
}

#[test]
fn description_rejects_schema_word_newline_and_trailing_punct() {
    assert!(matches!(sanitize_description("cfgd configuration schema"), Err(DescriptionError::ContainsSchemaWord)));
    assert!(matches!(sanitize_description("line one\nline two"), Err(DescriptionError::ContainsNewline)));
    assert!(matches!(sanitize_description("trailing comma,"), Err(DescriptionError::BadEdge)));
    assert!(matches!(sanitize_description("   "), Err(DescriptionError::Empty)));
    assert_eq!(sanitize_description("cfgd machine configuration").unwrap(), "cfgd machine configuration");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-stage-publish schemastore::tests::slugify`
Expected: FAIL — module/functions undefined.

- [ ] **Step 3: Implement** (`manifest.rs`)

```rust
//! Pure builders/validators for the schemastore publisher: slug, description
//! sanitization, `$schema`/`$id` checks, vendor JSON formatting, catalog-entry
//! construction. No I/O — every fn is unit-testable from a string.

/// Lowercase, trim, and replace runs of non-alphanumeric chars with a single `-`.
pub fn slugify(name: &str) -> String {
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
#[derive(Debug, PartialEq, Eq)]
pub enum DescriptionError { Empty, ContainsSchemaWord, ContainsNewline, BadEdge }

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

/// Validate a catalog `description` against SchemaStore's content rules. Returns
/// the trimmed description on success.
pub fn sanitize_description(desc: &str) -> Result<String, DescriptionError> {
    let trimmed = desc.trim();
    if trimmed.is_empty() { return Err(DescriptionError::Empty); }
    if desc.contains('\n') || desc.contains('\r') { return Err(DescriptionError::ContainsNewline); }
    if desc.to_ascii_lowercase().contains("schema") { return Err(DescriptionError::ContainsSchemaWord); }
    let bad = [',', '.', ' ', '\t', '-'];
    let first = desc.chars().next().unwrap();
    let last = desc.chars().last().unwrap();
    if bad.contains(&first) || bad.contains(&last) { return Err(DescriptionError::BadEdge); }
    Ok(trimmed.to_string())
}
```

In `mod.rs` add `pub(crate) mod manifest;` and `#[cfg(test)] mod tests;`. In `lib.rs` add `pub mod schemastore;`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-stage-publish schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/ crates/stage-publish/src/lib.rs
task commit -- -m "feat(schemastore): slug + description sanitization helpers"
```

---

## Task 5: `$schema` dialect classification + `$id` check (pure)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/manifest.rs`
- Modify: `crates/stage-publish/src/schemastore/tests.rs`

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::manifest::{classify_dialect, Dialect, check_id};

#[test]
fn dialect_draft07_ok_2020_12_too_high() {
    assert_eq!(classify_dialect("http://json-schema.org/draft-07/schema#"), Dialect::Ok);
    assert_eq!(classify_dialect("https://json-schema.org/draft-07/schema#"), Dialect::Ok);
    assert_eq!(classify_dialect("https://json-schema.org/draft/2020-12/schema"), Dialect::TooHigh);
    assert_eq!(classify_dialect("https://json-schema.org/draft/2019-09/schema"), Dialect::TooHigh);
    assert_eq!(classify_dialect("ftp://nonsense"), Dialect::Unknown);
}

#[test]
fn id_must_be_http() {
    assert!(check_id(Some("https://cfgd.io/schemas/cfgd-config.schema.json")).is_ok());
    assert!(check_id(Some("urn:bad")).is_err());
    assert!(check_id(None).is_err());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-stage-publish schemastore::tests::dialect`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement**

```rust
/// Result of classifying a schema's `$schema` against SchemaStore's CI gate.
#[derive(Debug, PartialEq, Eq)]
pub enum Dialect {
    /// draft-04/06/07 — accepted unconditionally.
    Ok,
    /// 2019-09 / 2020-12 — rejected unless allowlisted in `highSchemaVersion`.
    TooHigh,
    /// Not a recognized json-schema dialect URL.
    Unknown,
}

/// Classify a `$schema` URL. Mirrors SchemaStore's `SchemaDialects` table.
pub fn classify_dialect(schema_url: &str) -> Dialect {
    let u = schema_url.trim_end_matches('#');
    if u.contains("/draft-04/") || u.contains("/draft-06/") || u.contains("/draft-07/") {
        Dialect::Ok
    } else if u.contains("/draft/2019-09/") || u.contains("/draft/2020-12/") {
        Dialect::TooHigh
    } else {
        Dialect::Unknown
    }
}

/// SchemaStore requires `$id` to be an absolute http(s) URL.
pub fn check_id(id: Option<&str>) -> anyhow::Result<()> {
    match id {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => Ok(()),
        Some(s) => anyhow::bail!("schema `$id` must be an http(s) URL, got `{s}`"),
        None => anyhow::bail!("schema is missing a `$id` (SchemaStore requires an http(s) `$id`)"),
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-stage-publish schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/manifest.rs crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "feat(schemastore): \$schema dialect + \$id validation"
```

---

## Task 6: Idempotency verdict (pure)

**Files:**
- Create: `crates/stage-publish/src/schemastore/catalog.rs`
- Modify: `crates/stage-publish/src/schemastore/mod.rs` (`pub(crate) mod catalog;`)
- Modify: `crates/stage-publish/src/schemastore/tests.rs`

The catalog is a JSON object `{ "$schema": ..., "version": ..., "schemas": [ {entry}, ... ] }`. Entries are matched by `name`. Use the workspace's existing order-preserving JSON: `serde_json` with the `preserve_order` feature (confirm it's enabled in `stage-publish`/`core` Cargo.toml; krew uses `serde_yaml_ng` but catalog work needs JSON — if `preserve_order` is absent, add `serde_json = { version = "1", features = ["preserve_order"] }` to `crates/stage-publish/Cargo.toml`). Verdict logic only reads; splicing (Task 7) preserves bytes.

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::catalog::{verdict, Verdict};

const CATALOG: &str = r#"{ "schemas": [
  { "name": "Aaa", "description": "a", "fileMatch": ["a"], "url": "https://x/a.json" },
  { "name": "Anodizer", "description": "d", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" }
] }"#;

#[test]
fn verdict_noop_when_entry_present_and_equal() {
    let want = serde_json::json!({
        "name": "Anodizer", "description": "d",
        "fileMatch": [".anodizer.yaml"],
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, "Anodizer", &want).unwrap(), Verdict::NoOp);
}

#[test]
fn verdict_update_when_present_but_differs() {
    let want = serde_json::json!({ "name": "Anodizer", "description": "CHANGED", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" });
    assert_eq!(verdict(CATALOG, "Anodizer", &want).unwrap(), Verdict::Update);
}

#[test]
fn verdict_add_when_absent() {
    let want = serde_json::json!({ "name": "Zzz", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    assert_eq!(verdict(CATALOG, "Zzz", &want).unwrap(), Verdict::Add);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-stage-publish schemastore::tests::verdict`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement** (`catalog.rs`)

```rust
//! Pure operations on SchemaStore's `catalog.json` and `schema-validation.jsonc`.
//! Reads/edits are string-in/string-out so they unit-test without git or network.

use serde_json::Value;

/// What the publisher should do about one schema entry, given the upstream catalog.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict { NoOp, Add, Update }

/// Decide add/update/no-op by matching `name` in `catalog_json` against the
/// desired entry `want`. Comparison is structural (key order irrelevant).
pub fn verdict(catalog_json: &str, name: &str, want: &Value) -> anyhow::Result<Verdict> {
    let cat: Value = serde_json::from_str(catalog_json)?;
    let entries = cat.get("schemas").and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    match entries.iter().find(|e| e.get("name").and_then(Value::as_str) == Some(name)) {
        None => Ok(Verdict::Add),
        Some(existing) if json_eq(existing, want) => Ok(Verdict::NoOp),
        Some(_) => Ok(Verdict::Update),
    }
}

/// Structural equality ignoring object key order (serde_json::Value already
/// compares maps by content, so this is a thin wrapper for intent + future hooks).
fn json_eq(a: &Value, b: &Value) -> bool { a == b }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-stage-publish schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/catalog.rs crates/stage-publish/src/schemastore/mod.rs crates/stage-publish/src/schemastore/tests.rs crates/stage-publish/Cargo.toml
task commit -- -m "feat(schemastore): catalog idempotency verdict"
```

---

## Task 7: Surgical catalog splice (pure)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/catalog.rs`
- Modify: `crates/stage-publish/src/schemastore/tests.rs`

Critical correctness: do NOT reserialize the whole 1 MB file. Append a new entry before the closing `]` of the `schemas` array, or replace the matched entry's object span in place, leaving every other byte untouched. The inserted block uses canonical prettier key order (`$schema?, version?, name, description, fileMatch, url, versions?`), 2-space indent, double quotes, no trailing comma. **[CI-fact #1–4]**

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::catalog::{splice_entry, build_entry_json};

#[test]
fn splice_appends_without_touching_other_entries() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Aaa\",\n      \"description\": \"a\",\n      \"fileMatch\": [\"a\"],\n      \"url\": \"https://x/a.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "Zzz", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    let out = splice_entry(catalog, "Zzz", &entry).unwrap();
    assert!(out.contains("\"name\": \"Aaa\""), "existing entry preserved");
    assert!(out.contains("\"name\": \"Zzz\""), "new entry added");
    // existing entry block is byte-identical (only an inserted block + comma changed)
    assert!(out.contains("      \"description\": \"a\",\n      \"fileMatch\": [\"a\"],"));
    // valid JSON after splice
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

#[test]
fn splice_replaces_existing_entry_in_place() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Anodizer\",\n      \"description\": \"old\",\n      \"fileMatch\": [\".anodizer.yaml\"],\n      \"url\": \"https://u/old.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "Anodizer", "description": "new", "fileMatch": [".anodizer.yaml"], "url": "https://u/new.json" });
    let out = splice_entry(catalog, "Anodizer", &entry).unwrap();
    assert!(out.contains("\"description\": \"new\""));
    assert!(!out.contains("\"description\": \"old\""));
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

#[test]
fn build_entry_json_orders_keys_canonically() {
    let e = build_entry_json("Anodizer", "d", &[".anodizer.yaml".into()], "https://u/s.json", None);
    let s = serde_json::to_string(&e).unwrap();
    // name precedes description precedes fileMatch precedes url (preserve_order)
    let (np, dp, fp, up) = (s.find("name").unwrap(), s.find("description").unwrap(), s.find("fileMatch").unwrap(), s.find("url").unwrap());
    assert!(np < dp && dp < fp && fp < up);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --package anodizer-stage-publish schemastore::tests::splice`
Expected: FAIL — undefined.

- [ ] **Step 3: Implement**

```rust
use serde_json::{Map, Value};

/// Build a catalog entry object with keys in SchemaStore's prettier order.
/// `versions` is appended only when `Some`.
pub fn build_entry_json(
    name: &str, description: &str, file_match: &[String], url: &str,
    versions: Option<&Map<String, Value>>,
) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), Value::String(name.into()));
    m.insert("description".into(), Value::String(description.into()));
    m.insert("fileMatch".into(), Value::Array(file_match.iter().cloned().map(Value::String).collect()));
    m.insert("url".into(), Value::String(url.into()));
    if let Some(v) = versions { m.insert("versions".into(), Value::Object(v.clone())); }
    Value::Object(m)
}

/// Render an entry as a prettier-style block at the given indentation (number of
/// leading spaces for the object's `{`). Inner keys are indented `indent + 2`.
fn render_entry(entry: &Value, indent: usize) -> anyhow::Result<String> {
    // serde_json pretty-print uses 2-space; re-indent each line by `indent`.
    let pretty = serde_json::to_string_pretty(entry)?;
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for (i, line) in pretty.lines().enumerate() {
        if i > 0 { out.push('\n'); }
        out.push_str(&pad);
        out.push_str(line);
    }
    Ok(out)
}

/// Insert or replace the entry named `name`, preserving all other bytes.
/// Strategy: parse only to locate the entry; edit the raw string by byte span.
pub fn splice_entry(catalog: &str, name: &str, entry: &Value) -> anyhow::Result<String> {
    // Locate the `schemas` array and each entry's byte span via a streaming scan.
    // Implementation: find `"schemas"`, then walk balanced braces to enumerate
    // top-level entry object spans; for each, parse just that slice to read `name`.
    // - If a span's name == `name`: replace that span with render_entry(entry, span_indent).
    // - Else: insert before the array's closing `]`, adding a leading comma to the
    //   previous last entry. Match the indentation of sibling entries.
    // (Use the helper below; it returns the edited string.)
    splice_impl(catalog, name, entry)
}

fn splice_impl(catalog: &str, name: &str, entry: &Value) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(catalog)?;
    let arr = v.get("schemas").and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("catalog.json has no `schemas` array"))?;
    let entry_indent = 4usize; // SchemaStore: 2 for `schemas`, 4 for each entry object

    // REPLACE: find the existing object's exact text span and swap it.
    if arr.iter().any(|e| e.get("name").and_then(Value::as_str) == Some(name)) {
        let (start, end) = find_entry_span(catalog, name)?;
        let rendered = render_entry(entry, entry_indent);
        let rendered = rendered.trim_start(); // span already starts at the `{`'s indent
        let mut out = String::with_capacity(catalog.len());
        out.push_str(&catalog[..start]);
        out.push_str(rendered);
        out.push_str(&catalog[end..]);
        return Ok(out);
    }

    // APPEND: insert before the array's closing `]`, comma-joining.
    let close = find_array_close(catalog)?; // byte index of the `]` that ends `schemas`
    let before = catalog[..close].trim_end();
    let needs_comma = before.ends_with('}');
    let rendered = render_entry(entry, entry_indent);
    let mut out = String::with_capacity(catalog.len() + rendered.len() + 2);
    out.push_str(before);
    if needs_comma { out.push(','); }
    out.push('\n');
    out.push_str(&rendered);
    out.push('\n');
    out.push_str("  "); // indent for the closing `]`
    out.push_str(&catalog[close..]);
    Ok(out)
}
```

Implement `find_entry_span(catalog, name) -> (start_byte, end_byte)` and `find_array_close(catalog) -> usize` as brace-balanced scanners over the `"schemas"` array region (track string/escape state so braces inside strings don't count). Add focused tests for both scanners (nested braces in a `$comment`, escaped quotes). Keep them in `catalog.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --package anodizer-stage-publish schemastore::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/catalog.rs crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "feat(schemastore): surgical catalog splice (append + in-place)"
```

---

## Task 8: `versions` merge (pure)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/catalog.rs`, `tests.rs`

Versioned mode: `url` points at `<slug>-<VER>.json`; the new `<VER>` is merged into the upstream entry's existing `versions` map (carry prior versions forward). **[CI-fact #7]**

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::catalog::merge_versions;

#[test]
fn merge_versions_carries_prior_and_adds_new() {
    let mut prior = serde_json::Map::new();
    prior.insert("1.2".into(), serde_json::json!("https://www.schemastore.org/cfgd-config-1.2.json"));
    let merged = merge_versions(Some(&prior), "1.3", "https://www.schemastore.org/cfgd-config-1.3.json");
    assert_eq!(merged.get("1.2").unwrap(), "https://www.schemastore.org/cfgd-config-1.2.json");
    assert_eq!(merged.get("1.3").unwrap(), "https://www.schemastore.org/cfgd-config-1.3.json");
}

#[test]
fn merge_versions_from_empty() {
    let merged = merge_versions(None, "1.0.0", "https://www.schemastore.org/x-1.0.0.json");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged.get("1.0.0").unwrap(), "https://www.schemastore.org/x-1.0.0.json");
}
```

- [ ] **Step 2: Run** — Expected FAIL (undefined).

- [ ] **Step 3: Implement**

```rust
/// Merge a new version into an existing `versions` map (or start fresh),
/// carrying all prior versions forward.
pub fn merge_versions(
    prior: Option<&serde_json::Map<String, serde_json::Value>>,
    version: &str, url: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = prior.cloned().unwrap_or_default();
    m.insert(version.to_string(), serde_json::Value::String(url.to_string()));
    m
}
```

- [ ] **Step 4: Run** — Expected PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/catalog.rs crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "feat(schemastore): versions map merge"
```

---

## Task 9: `highSchemaVersion` allowlist edit (pure)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/catalog.rs`, `tests.rs`

When a vendored schema is draft-2020-12/2019-09, add its name to the `highSchemaVersion` array in `src/schema-validation.jsonc` (a JSONC file — preserve comments via textual splice, do not reserialize). **[CI-fact #5]**

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::catalog::add_high_schema_version;

#[test]
fn adds_name_to_high_schema_version_array() {
    let jsonc = "{\n  // comment\n  \"highSchemaVersion\": [\n    \"existing-2020\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert!(out.contains("// comment"), "comments preserved");
    assert!(out.contains("\"existing-2020\""));
    assert!(out.contains("\"cfgd-module\""));
}

#[test]
fn idempotent_when_already_present() {
    let jsonc = "{\n  \"highSchemaVersion\": [\n    \"cfgd-module\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert_eq!(out.matches("\"cfgd-module\"").count(), 1);
}
```

- [ ] **Step 2: Run** — Expected FAIL.

- [ ] **Step 3: Implement** — locate `"highSchemaVersion"`, find its `[`, if the name isn't already an element insert it as a new line before the closing `]`, matching element indentation and comma-joining. Reuse the brace/bracket scanner from Task 7 (extract a shared `find_bracket_close`).

- [ ] **Step 4: Run** — Expected PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/catalog.rs crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "feat(schemastore): highSchemaVersion allowlist edit"
```

---

## Task 10: Vendor JSON formatting (pure)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/manifest.rs`, `tests.rs`

Vendored `src/schemas/json/<slug>.json` must be prettier-clean: 2-space, double quotes, trailing newline, keys preserved as authored (prettier-plugin-sort-json does NOT apply to `src/schemas/json/**`, only to catalog — confirm in `.prettierrc.cjs`; if it does, sort accordingly). Serialize via `serde_json::to_string_pretty` + trailing `\n`.

- [ ] **Step 1: Write the failing test**

```rust
use crate::schemastore::manifest::format_vendor_schema;

#[test]
fn format_vendor_schema_is_2space_with_trailing_newline() {
    let raw = "{\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"type\":\"object\"}";
    let out = format_vendor_schema(raw).unwrap();
    assert!(out.ends_with("}\n"));
    assert!(out.contains("\n  \"type\": \"object\""));
}
```

- [ ] **Step 2: Run** — Expected FAIL.

- [ ] **Step 3: Implement**

```rust
/// Reformat a schema's JSON to SchemaStore's prettier defaults (2-space indent,
/// trailing newline). Preserves key order (serde_json `preserve_order`).
pub fn format_vendor_schema(raw: &str) -> anyhow::Result<String> {
    let v: serde_json::Value = serde_json::from_str(raw)?;
    let mut s = serde_json::to_string_pretty(&v)?;
    s.push('\n');
    Ok(s)
}
```

- [ ] **Step 4: Run** — Expected PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/manifest.rs crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "feat(schemastore): vendor schema formatting"
```

---

## Task 11: Publisher skeleton + preflight

**Files:**
- Modify: `crates/stage-publish/src/schemastore/mod.rs`

Read `crates/stage-publish/src/krew.rs` lines around `simple_publisher!(KrewPublisher, ...)` and its `impl Publisher for KrewPublisher` first. Mirror it.

- [ ] **Step 1: Write the failing test** (registry/dispatch-level, in `mod.rs` tests)

```rust
#[test]
fn publisher_identity_is_manager_group_not_required_by_default() {
    let p = SchemastorePublisher::new();
    assert_eq!(p.name(), "schemastore");
    assert_eq!(p.group(), anodizer_core::PublisherGroup::Manager);
    assert!(!p.required());
    assert!(p.skips_on_nightly());
}
```

- [ ] **Step 2: Run** — Expected FAIL (undefined).

- [ ] **Step 3: Implement** the macro + trait impl. `run()` is a stub returning `Ok(PublishEvidence::new("schemastore"))` for now (Task 13 fills it). `preflight()` validates every effective schema (`entry.validate()`, description sanitize when present, dialect/`$id` for vendor by reading `schema_file` off disk) and, for external, performs the URL reachability GET; return `Blocker`/`Warning` accordingly.

```rust
use anodizer_core::{Context, PublishEvidence, PublisherGroup};
use anodizer_core::publisher::PreflightCheck;

simple_publisher!(
    SchemastorePublisher,
    "schemastore",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

impl anodizer_core::Publisher for SchemastorePublisher {
    fn name(&self) -> &str { Self::PUBLISHER_NAME }
    fn group(&self) -> PublisherGroup { PublisherGroup::Manager }
    fn required(&self) -> bool { self.resolved_required() }
    fn skips_on_nightly(&self) -> bool { true }
    fn rollback_scope_needed(&self) -> Option<&'static str> { Some("GITHUB_TOKEN pull_request:write") }

    fn preflight(&self, ctx: &Context) -> anyhow::Result<PreflightCheck> {
        // Validate each effective (non-skipped) schema; return Blocker on the
        // first hard failure. See manifest:: validators + dialect/$id for vendor.
        super::schemastore::preflight_checks(ctx)
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        super::schemastore::run_publish(ctx) // Task 13
    }

    fn rollback(&self, ctx: &mut Context, evidence: &PublishEvidence) -> anyhow::Result<()> {
        super::schemastore::rollback_publish(ctx, evidence) // Task 14
    }
}
```

> Confirm the exact associated-const names (`PUBLISHER_NAME`, `PUBLISHER_REQUIRED`, `resolved_required`) the `simple_publisher!` macro generates by reading `publisher_helpers.rs:191`. Use whatever krew uses.

- [ ] **Step 4: Run** — Expected PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/mod.rs
task commit -- -m "feat(schemastore): publisher skeleton + preflight"
```

---

## Task 12: Registry wiring

**Files:**
- Modify: `crates/stage-publish/src/registry.rs`

- [ ] **Step 1: Write the failing test** (mirror an existing `is_*_configured` test in `registry.rs` tests)

```rust
#[test]
fn schemastore_registers_in_manager_group_when_schemas_present() {
    let mut cfg = anodizer_core::config::Config::default();
    cfg.schemastore.schemas.push(anodizer_core::config::publishers::SchemaEntry {
        name: "Anodizer".into(),
        file_match: vec![".anodizer.yaml".into()],
        url: Some("https://x/s.json".into()),
        ..Default::default()
    });
    let ctx = test_ctx_with_config(cfg); // use the existing registry-test helper
    let pubs = configured_publishers(&ctx);
    assert!(pubs.iter().any(|p| p.name() == "schemastore" && p.group() == anodizer_core::PublisherGroup::Manager));
}
```

- [ ] **Step 2: Run** — Expected FAIL.

- [ ] **Step 3: Implement** — add near the mcp/dockerhub blocks in `configured_publishers()`:

```rust
if is_schemastore_configured(ctx) {
    let req = collapse_required(ctx.config.schemastore.schemas.iter().map(|s| s.required));
    v.push(Box::new(crate::schemastore::SchemastorePublisher::with_required(req)));
}
```

and the predicate:

```rust
/// `schemastore:` is active when the block carries at least one schema entry.
fn is_schemastore_configured(ctx: &Context) -> bool {
    !ctx.config.schemastore.schemas.is_empty()
}
```

- [ ] **Step 4: Run** — Expected PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/registry.rs
task commit -- -m "feat(schemastore): register publisher in Manager group"
```

---

## Task 13: `run()` orchestration

**Files:**
- Modify: `crates/stage-publish/src/schemastore/mod.rs`

Integration-level; model the git/PR sequence on krew's `run()`. Keep `run_publish` thin — delegate every decision to the Task 4–10 helpers.

Sequence (each a clear sub-step; cite the krew call sites you reuse):
1. Collect effective schemas (skip ones where `resolved_skip` or `should_skip_publisher_with_if` is true). If none → `Ok(PublishEvidence::new("schemastore"))`.
2. Dry-run/snapshot (`ctx.is_dry_run() || ctx.is_snapshot()`) → log the planned per-schema verdict + diff, open no PR, return evidence.
3. Resolve the fork `RepositoryConfig` (block-level). Clone/checkout via the same util krew uses; **sync to upstream master** (fetch `https://github.com/SchemaStore/schemastore` master, reset the work branch onto it) so edits target the probe's tree. **[CI-fact #8]**
4. Read upstream `src/api/json/catalog.json` (from the synced checkout) once.
5. For each schema: build the desired entry (`build_entry_json`, description via `meta_description_project()`/`meta_description_for(crate)` + `sanitize_description`; vendor `url` = `https://www.schemastore.org/<slug>.json`, external `url` = config). Compute `verdict`. For vendor, also bind the crate version via `with_published_crate_scope` when `versioned`. Apply `splice_entry`; for vendor write the formatted file + (if too-high) `add_high_schema_version`; for versioned `merge_versions`.
6. Also check open fork→upstream PRs touching these paths (pending PR ⇒ treat as PendingValidation, skip add). **[CI-fact W1]**
7. If every schema is NoOp and no pending PR work → return evidence with no PR (anodizer dogfood result).
8. Commit (author cascade via `util/commit.rs`), push the branch, open/refresh the PR via `util/pr.rs` against base `SchemaStore/schemastore@master`. Record PR target(s) in `PublishEvidence.extra` for rollback (mirror krew's `KrewTargetSnapshot`).

- [ ] **Step 1: Write the failing test** — dry-run path only (no network):

```rust
#[test]
fn dry_run_reports_noop_for_existing_external_entry_and_opens_no_pr() {
    // TestContextBuilder with is_dry_run, schemastore.schemas = [Anodizer external].
    // Assert run_publish returns Ok, evidence has no PR target, and the log
    // contains a "no-op"/"would" line. (Use the crate's existing test harness.)
}
```

- [ ] **Step 2–4:** implement `run_publish`/`preflight_checks`; run the dry-run test → PASS. Add an integration test under `crates/stage-publish/tests/` if a fixture catalog can be injected without network.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/mod.rs crates/stage-publish/tests/
task commit -- -m "feat(schemastore): run() orchestration (sync, probe, splice, PR)"
```

---

## Task 14: `rollback()`

**Files:**
- Modify: `crates/stage-publish/src/schemastore/mod.rs`

Mirror krew's close-PR rollback: decode PR targets from evidence, dedup by `(repo_url, branch)`, close each via `gh`/REST, best-effort. If a target's PR is already merged, log that a revert PR is required (cannot close a merged PR).

- [ ] **Step 1: Write the failing test** — empty-evidence path uses `rollback_empty_warning_msg("schemastore", "pull request")`:

```rust
#[test]
fn rollback_with_no_targets_is_noop_warning() {
    let p = SchemastorePublisher::new();
    let ev = PublishEvidence::new("schemastore");
    let mut ctx = /* minimal test ctx */;
    assert!(p.rollback(&mut ctx, &ev).is_ok());
}
```

- [ ] **Step 2–4:** implement; run → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/mod.rs
task commit -- -m "feat(schemastore): close-PR rollback"
```

---

## Task 15: Mode-axis test (single / lockstep / per-crate)

**Files:**
- Modify: `crates/stage-publish/src/schemastore/tests.rs` (or `crates/stage-publish/tests/`)

Mandatory anodizer axis. The risk is vendor/versioned in per-crate mode stamping crate[0]'s version. **[arch-fact BLOCKER 1]**

- [ ] **Step 1: Write the test** — build a workspace per-crate context (two crates, independent tags, e.g. `cfgd-core@v1.0.0`, `cfgd@v2.0.0`); a versioned vendor schema with `crate: cfgd`; assert the produced `versions` key / `<slug>-<VER>.json` filename is `2.0.0`, NOT `1.0.0`. Add single-crate + lockstep variants asserting the single tag's version.
- [ ] **Step 2: Run** — Expected FAIL if `with_published_crate_scope` isn't applied (red proves the bug).
- [ ] **Step 3:** ensure Task 13's versioned path wraps version resolution in `with_published_crate_scope(ctx, crate, ...)`.
- [ ] **Step 4: Run** — Expected PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/schemastore/tests.rs
task commit -- -m "test(schemastore): per-crate version scope across config modes"
```

---

## Task 16: Regenerate `schema.json`

**Files:**
- Modify: `docs/site/public/schema.json`, `docs/site/static/schema.json`

- [ ] **Step 1:** run the repo's schema-gen task (check `Taskfile.yml` — likely `task gen-schema` or part of `task lint`'s snapshot step; the recent commit "regenerate schema" shows the path).

Run: `task --list | grep -i schema` then the matching target.

- [ ] **Step 2:** confirm the diff adds `schemastore` under the config schema and nothing unexpected.
- [ ] **Step 3: Commit**

```bash
git add docs/site/public/schema.json docs/site/static/schema.json
task commit -- -m "docs(schema): regenerate for schemastore publisher"
```

---

## Task 17: anodizer user docs

**Files:**
- Create: `docs/site/content/docs/publish/schemastore.md`
- Modify: publish-section nav (find how `krew.md`/`aur.md` are listed — `docs/site/content/docs/publish/_index.md` or a zola `weight`/menu); `docs/site/content/docs/publish/before-publish.md` (cross-link)

- [ ] **Step 1:** write `schemastore.md` modeled on `krew.md`: front matter (title/weight), intro, the two modes with YAML examples (the spec's config block), the cascade, `file_match`/`crate`/`versioned`, the SchemaStore content rules consumers must respect (no "schema" in description, draft-07 preferred, `**/` folder globs), and the dogfood note. Affirmative voice ("anodizer registers your schema on SchemaStore"), not GoReleaser-relative.
- [ ] **Step 2:** add nav entry + cross-link.
- [ ] **Step 3:** build the docs site if there's a `task docs`/zola build target; confirm no broken links.
- [ ] **Step 4: Commit**

```bash
git add docs/site/content/docs/publish/schemastore.md docs/site/content/docs/publish/before-publish.md docs/site/content/docs/publish/_index.md
task commit -- -m "docs: document the schemastore publisher"
```

---

## Task 18: anodizer-action token docs (SEPARATE REPO — gated)

**Files (in `/opt/repos/anodizer-action`):**
- Modify: `action.yml`, `README.md`

Do NOT start until the user approves working in `anodizer-action` (per-repo push rule). The publisher needs a PAT that can push the fork + open the upstream PR.

- [ ] **Step 1:** read `action.yml` to see how `HOMEBREW_TAP_TOKEN`/publisher tokens are surfaced. Document the SchemaStore fork token the same way (input/env passthrough), noting the required scopes (`public_repo`/`pull_request:write` on the fork + upstream).
- [ ] **Step 2:** mirror it in `README.md` (the secrets table).
- [ ] **Step 3: Commit** (in that repo; non-master branch push is pre-approved, master is not):

```bash
cd /opt/repos/anodizer-action && git add action.yml README.md
git commit -m "docs: document SchemaStore fork token"
```

---

## Task 19: cfgd vendor config + docs (SEPARATE REPO — gated)

**Files (in `/opt/repos/cfgd`):**
- Modify: `.anodizer.yaml` (add the `schemastore:` block, vendor mode, 4 schemas, `crate: cfgd`)
- Modify: cfgd release docs
- Possibly: regenerate `cfgd-module.schema.json` as draft-07 if the team prefers that to the allowlist path

Do NOT start until the user approves working in `cfgd`.

- [ ] **Step 1:** add the block:

```yaml
schemastore:
  repository: { owner: tj-smith47, name: schemastore }
  schemas:
    - { name: cfgd-config,  slug: cfgd-config,  crate: cfgd, file_match: ["cfgd.yaml", ".cfgd.yaml"], schema_file: "schemas/cfgd-config.schema.json",  description: "cfgd machine configuration" }
    - { name: cfgd-module,  slug: cfgd-module,  crate: cfgd, file_match: ["**/modules/*.yaml"],        schema_file: "schemas/cfgd-module.schema.json",  description: "cfgd module definition" }
    - { name: cfgd-source,  slug: cfgd-source,  crate: cfgd, file_match: ["**/sources/*.yaml"],        schema_file: "schemas/cfgd-source.schema.json",  description: "cfgd config source" }
    - { name: cfgd-profile, slug: cfgd-profile, crate: cfgd, file_match: ["**/profiles/*.yaml"],       schema_file: "schemas/cfgd-profile.schema.json", description: "cfgd profile" }
```

> Verify each `description` avoids the literal word "schema" and each `file_match` is what cfgd actually uses. `cfgd-module` is draft-2020-12 → the publisher auto-allowlists it; confirm that lands in the PR.

- [ ] **Step 2:** dry-run `anodizer release --snapshot` (or the dogfood invocation) against cfgd; confirm the planned PR = 4 vendored files + 4 catalog entries + 1 `highSchemaVersion` add. Capture the real output.
- [ ] **Step 3:** document in cfgd's release docs.
- [ ] **Step 4: Commit** (in cfgd; non-master push pre-approved).

---

## Self-review (run after writing — done)

- **Spec coverage:** external + vendor modes (T1,3,7,10,13); cascade (T2); validation incl. description/dialect/`$id`/neither-both (T3,4,5,11); catalog non-alphabetical splice + prettier order (T7); versions merge (T8); allowlist (T9); fork sync + pending-PR idempotency (T6,13); crate-version scope across modes (T15); registry collapse_required (T12); rollback (T14); schema regen (T16); docs for all three repos (T17,18,19). All spec sections map to a task.
- **Placeholder scan:** integration Tasks 13/14 intentionally specify a sequence + the helpers to reuse rather than full code, because they wrap krew's ~200-line git/PR machinery — the plan names the exact reuse points and red-test to write. Pure helpers (T4–10) carry complete code.
- **Type consistency:** `SchemastoreConfig`/`SchemaEntry`/`SchemaMode`/`Verdict`/`Dialect`/`DescriptionError`; `slugify`/`sanitize_description`/`classify_dialect`/`check_id`/`verdict`/`splice_entry`/`build_entry_json`/`merge_versions`/`add_high_schema_version`/`format_vendor_schema` — names are stable across all tasks. `crate_` (serde rename `crate`) used consistently.

## Execution handoff

Implement Tasks 1–17 in this repo via subagent-driven-development. Tasks 18–19 are separate repos — start only on the user's per-repo go.
