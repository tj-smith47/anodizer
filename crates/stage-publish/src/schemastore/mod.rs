//! SchemaStore publisher: registers a tool's JSON Schema(s) on
//! [SchemaStore](https://www.schemastore.org/) via a pull request against a
//! fork of `SchemaStore/schemastore`, plus the pure helpers (slug, description
//! validation, catalog-entry construction, JSON vendor formatting) it builds on.

// The publisher struct, its `impl Publisher`, and the preflight/run/rollback
// free functions are constructed by the publisher registry, which is the only
// production call site; the registry lives outside this module. A
// module-scoped allow keeps that surface from tripping `dead_code` /
// `-D warnings` without scattering per-item attributes a later edit could
// leave stale.
#![allow(dead_code)]

pub(crate) mod catalog;
pub(crate) mod manifest;
pub(crate) mod scan;

#[cfg(test)]
mod tests;

use anodizer_core::config::SchemaMode;
use anodizer_core::context::Context;
use anodizer_core::{PreflightCheck, PublishEvidence, PublisherGroup};

// Manager group: like krew/homebrew/scoop this pushes to a long-lived
// community index whose nightly clobber is disruptive, so `skips_on_nightly`
// is true. `required` defaults false so a release still succeeds if the
// registration PR cannot be opened; the per-entry config `required` overrides
// it.
simple_publisher!(
    SchemastorePublisher,
    "schemastore",
    PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN pull_request:write"),
);

impl anodizer_core::Publisher for SchemastorePublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn preflight(&self, ctx: &Context) -> anyhow::Result<PreflightCheck> {
        preflight_checks(ctx)
    }
    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        run_publish(ctx)
    }
    fn rollback(&self, ctx: &mut Context, evidence: &PublishEvidence) -> anyhow::Result<()> {
        rollback_publish(ctx, evidence)
    }
}

/// Validate every non-skipped schema entry before the publish stage runs.
///
/// Per entry, in order: config-shape (`validate`), description content rules,
/// and — for vendor mode — that the schema file exists on disk, parses as
/// JSON, carries an http(s) `$id`, and uses a recognized json-schema dialect.
/// External entries are checked only for a well-formed http(s) `url`.
///
/// Aggregation is first-blocker-wins: the first [`PreflightCheck::Blocker`]
/// short-circuits and is returned; absent any blocker the first
/// [`PreflightCheck::Warning`] is returned; otherwise [`PreflightCheck::Pass`].
fn preflight_checks(ctx: &Context) -> anyhow::Result<PreflightCheck> {
    let cfg = &ctx.config.schemastore;
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let mut first_warning: Option<String> = None;

    for entry in &cfg.schemas {
        if cfg.resolved_skip(entry) {
            continue;
        }

        if let Err(e) = entry.validate() {
            return Ok(PreflightCheck::Blocker(format!(
                "schemastore: schema `{}`: {e}",
                entry.name
            )));
        }

        if let Some(desc) = entry.description.as_deref()
            && let Err(e) = manifest::sanitize_description(desc)
        {
            return Ok(PreflightCheck::Blocker(format!(
                "schemastore: schema `{}` description: {e}",
                entry.name
            )));
        }

        match entry.mode()? {
            SchemaMode::Vendor => {
                if let Some(check) = preflight_vendor(&root, entry, &mut first_warning) {
                    return Ok(check);
                }
            }
            SchemaMode::External => {
                preflight_external(entry, &mut first_warning);
            }
        }
    }

    if let Some(w) = first_warning {
        return Ok(PreflightCheck::Warning(w));
    }
    Ok(PreflightCheck::Pass)
}

/// Vendor-entry preflight: read the schema file off disk, parse it, and check
/// `$schema`/`$id`. Returns `Some(Blocker)` to short-circuit the whole
/// preflight; returns `None` (recording at most a warning in `first_warning`)
/// to continue to the next entry.
fn preflight_vendor(
    root: &std::path::Path,
    entry: &anodizer_core::config::SchemaEntry,
    first_warning: &mut Option<String>,
) -> Option<PreflightCheck> {
    // `mode() == Vendor` guarantees `schema_file` is `Some`.
    let rel = entry.schema_file.as_deref()?;
    let path = root.join(rel);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Some(PreflightCheck::Blocker(format!(
                "schemastore: schema `{}`: cannot read schema_file `{}` ({e})",
                entry.name,
                path.display()
            )));
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Some(PreflightCheck::Blocker(format!(
                "schemastore: schema `{}`: schema_file `{}` is not valid JSON ({e})",
                entry.name,
                path.display()
            )));
        }
    };

    let id = json.get("$id").and_then(serde_json::Value::as_str);
    if let Err(e) = manifest::check_id(id) {
        return Some(PreflightCheck::Blocker(format!(
            "schemastore: schema `{}`: {e}",
            entry.name
        )));
    }

    if let Some(schema_url) = json.get("$schema").and_then(serde_json::Value::as_str)
        && manifest::classify_dialect(schema_url) == manifest::Dialect::Unknown
    {
        first_warning.get_or_insert(format!(
            "schemastore: schema `{}`: unrecognized `$schema` dialect `{schema_url}` \
             (SchemaStore CI may reject it)",
            entry.name
        ));
    }

    None
}

/// External-entry preflight: validate the `url` is a well-formed http(s) URL.
///
/// Reachability is deliberately NOT probed: anodizer may be releasing the very
/// site that will host the schema, so the URL is legitimately not live until
/// after this release completes. An unreachable (or here, merely malformed)
/// external URL is therefore at most a warning, never a blocker.
fn preflight_external(
    entry: &anodizer_core::config::SchemaEntry,
    first_warning: &mut Option<String>,
) {
    // `mode() == External` guarantees `url` is `Some`.
    let Some(url) = entry.url.as_deref() else {
        return;
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        first_warning.get_or_insert(format!(
            "schemastore: schema `{}`: url `{url}` is not a well-formed http(s) URL",
            entry.name
        ));
    }
}

/// Run the SchemaStore publish, returning evidence of what was registered.
///
/// Currently a no-op that returns empty evidence: the catalog-splice and PR
/// pipeline is built on the pure helpers in `catalog`/`scan`/`manifest` but
/// not yet driven from here.
fn run_publish(_ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
    Ok(PublishEvidence::new("schemastore"))
}

/// Roll back a SchemaStore publish given its evidence. Currently a no-op: the
/// PR-revert path has no recorded targets to act on yet.
fn rollback_publish(_ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{SchemaEntry, SchemastoreConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};

    #[test]
    fn publisher_identity_is_manager_group_not_required_by_default() {
        let p = SchemastorePublisher::new();
        assert_eq!(p.name(), "schemastore");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert!(p.skips_on_nightly());
    }

    #[test]
    fn publisher_declares_rollback_scope() {
        let p = SchemastorePublisher::new();
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN pull_request:write")
        );
    }

    #[test]
    fn preflight_passes_with_no_schemas() {
        let ctx = TestContextBuilder::new().build();
        let p = SchemastorePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_file_missing() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/does-not-exist.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("does-not-exist.json"), "{msg}");
            }
            other => panic!("expected Blocker for missing vendor file, got {other:?}"),
        }
    }

    #[test]
    fn preflight_warns_on_malformed_external_url() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("ftp://example.com/a.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("http(s)"), "{msg}");
            }
            other => panic!("expected Warning for malformed url, got {other:?}"),
        }
    }

    #[test]
    fn preflight_passes_for_well_formed_external_url() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = SchemastorePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_on_bad_description() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                url: Some("https://example.com/a.json".into()),
                description: Some("a schema for stuff".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
            }
            other => panic!("expected Blocker for bad description, got {other:?}"),
        }
    }

    #[test]
    fn preflight_skips_disabled_entries() {
        use anodizer_core::config::StringOrBool;
        let mut ctx = TestContextBuilder::new().build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/does-not-exist.json".into()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = SchemastorePublisher::new();
        // The would-be Blocker (missing vendor file) must be skipped.
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    /// Write `body` to `<root>/schemas/anodizer.schema.json` and return a
    /// context whose `project_root` is `root` with a single vendor entry
    /// pointing at it. Used by the vendor-on-disk preflight tests.
    fn vendor_ctx_with_schema(root: &std::path::Path, body: &str) -> Context {
        let schemas_dir = root.join("schemas");
        std::fs::create_dir_all(&schemas_dir).expect("mkdir schemas");
        std::fs::write(schemas_dir.join("anodizer.schema.json"), body).expect("write schema");
        let mut ctx = TestContextBuilder::new()
            .project_root(root.to_path_buf())
            .build();
        ctx.config.schemastore = SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".into(),
                file_match: vec![".anodizer.yaml".into()],
                schema_file: Some("schemas/anodizer.schema.json".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        ctx
    }

    #[test]
    fn preflight_passes_for_valid_vendor_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://example.com/anodizer.schema.json",
  "type": "object"
}"#,
        );
        let p = SchemastorePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_missing_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object"
}"#,
        );
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("$id"), "{msg}");
            }
            other => panic!("expected Blocker for missing $id, got {other:?}"),
        }
    }

    #[test]
    fn preflight_warns_for_unknown_dialect_vendor_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(
            dir.path(),
            r#"{
  "$schema": "https://example.com/not-a-known-dialect#",
  "$id": "https://example.com/anodizer.schema.json",
  "type": "object"
}"#,
        );
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Warning(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("dialect"), "{msg}");
            }
            other => panic!("expected Warning for unknown dialect, got {other:?}"),
        }
    }

    #[test]
    fn preflight_blocks_when_vendor_schema_not_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = vendor_ctx_with_schema(dir.path(), "this is not json {");
        let p = SchemastorePublisher::new();
        match p.preflight(&ctx).expect("preflight ok") {
            PreflightCheck::Blocker(msg) => {
                assert!(msg.contains("Anodizer"), "{msg}");
                assert!(msg.contains("not valid JSON"), "{msg}");
            }
            other => panic!("expected Blocker for non-JSON schema, got {other:?}"),
        }
    }

    #[test]
    fn run_and_rollback_are_nonpanicking_stubs() {
        let mut ctx = TestContextBuilder::new().build();
        let p = SchemastorePublisher::new();
        let ev = p.run(&mut ctx).expect("run stub ok");
        assert_eq!(ev.publisher, "schemastore");
        assert!(p.rollback(&mut ctx, &ev).is_ok());
    }
}
