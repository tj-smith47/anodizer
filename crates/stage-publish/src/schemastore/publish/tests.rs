use super::super::catalog;
use super::super::manifest::{self, Dialect};
use super::*;
use anodizer_core::config::{RepositoryConfig, SchemaEntry, SchemaMode, SchemastoreConfig};
use anodizer_core::log::StageLogger;
use anodizer_core::test_helpers::TestContextBuilder;
use serde_json::Value;
use std::path::PathBuf;

fn external_entry() -> SchemaEntry {
    SchemaEntry {
        name: "Anodizer".into(),
        file_match: vec![".anodizer.yaml".into(), ".anodizer.yml".into()],
        url: Some("https://tj-smith47.github.io/anodizer/schema.json".into()),
        description: Some("Anodizer Rust release-automation configuration file".into()),
        ..Default::default()
    }
}

fn vendor_entry() -> SchemaEntry {
    SchemaEntry {
        name: "cfgd-config".into(),
        file_match: vec!["cfgd.yaml".into()],
        schema_file: Some("schemas/cfgd-config.schema.json".into()),
        description: Some("cfgd machine configuration".into()),
        ..Default::default()
    }
}

/// A minimal upstream catalog containing only the entry that the test
/// builds, so `verdict` can resolve to NoOp/Update/Add deterministically.
fn catalog_with(entries: &[Value]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "../../schema-catalog.json",
        "version": 1.0,
        "schemas": entries,
    }))
    .unwrap()
}

// --- plan_schema (pure) ---------------------------------------------

#[test]
fn plan_external_uses_entry_url_and_no_vendor_file() {
    let e = external_entry();
    let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
    assert_eq!(plan.mode, SchemaMode::External);
    assert_eq!(
        plan.url,
        "https://tj-smith47.github.io/anodizer/schema.json"
    );
    assert!(plan.vendor_path.is_none());
    assert!(plan.verdict.is_none(), "no catalog provided ⇒ no verdict");
}

#[test]
fn plan_vendor_derives_slug_url_and_path() {
    let e = vendor_entry();
    let plan = plan_schema(&e, "cfgd machine config", false, None, None).unwrap();
    assert_eq!(plan.mode, SchemaMode::Vendor);
    assert_eq!(plan.url, "https://www.schemastore.org/cfgd-config.json");
    assert_eq!(
        plan.vendor_path.unwrap(),
        PathBuf::from("src/schemas/json/cfgd-config.json")
    );
}

#[test]
fn plan_versioned_vendor_stamps_version_into_filename_and_url() {
    let e = vendor_entry();
    let plan = plan_schema(&e, "cfgd machine config", true, Some("0.4.2"), None).unwrap();
    assert_eq!(
        plan.url,
        "https://www.schemastore.org/cfgd-config-0.4.2.json"
    );
    assert_eq!(
        plan.vendor_path.unwrap(),
        PathBuf::from("src/schemas/json/cfgd-config-0.4.2.json")
    );
    // The versions map carries the new version → versioned url.
    let versions = plan
        .desired_entry
        .get("versions")
        .and_then(Value::as_object);
    assert_eq!(
        versions
            .and_then(|m| m.get("0.4.2"))
            .and_then(Value::as_str),
        Some("https://www.schemastore.org/cfgd-config-0.4.2.json")
    );
}

#[test]
fn plan_versioned_vendor_merges_prior_versions_forward() {
    let e = vendor_entry();
    // Upstream already lists 0.4.1; the new 0.4.2 must not drop it.
    let prior = serde_json::json!({
        "name": "cfgd-config",
        "description": "cfgd machine configuration",
        "fileMatch": ["cfgd.yaml"],
        "url": "https://www.schemastore.org/cfgd-config-0.4.1.json",
        "versions": { "0.4.1": "https://www.schemastore.org/cfgd-config-0.4.1.json" },
    });
    let cat = catalog_with(&[prior]);
    let plan = plan_schema(
        &e,
        "cfgd machine configuration",
        true,
        Some("0.4.2"),
        Some(&cat),
    )
    .unwrap();
    let versions = plan
        .desired_entry
        .get("versions")
        .and_then(Value::as_object)
        .unwrap();
    assert!(
        versions.contains_key("0.4.1"),
        "prior version carried forward"
    );
    assert!(versions.contains_key("0.4.2"), "new version added");
}

/// Prior versions must carry forward even when the upstream entry's `name`
/// drifts in case: the lookup matches on `fileMatch`-overlap, not `name`,
/// so a name-keyed lookup (which would miss this entry, reset the map, and
/// drop older versioned URLs SchemaStore CI then rejects) is regressed
/// against here.
#[test]
fn plan_versioned_vendor_carries_prior_versions_despite_name_case_drift() {
    let e = vendor_entry(); // desired name "cfgd-config", fileMatch ["cfgd.yaml"]
    let prior = serde_json::json!({
        "name": "Cfgd-Config", // upstream drifted in case
        "description": "cfgd machine configuration",
        "fileMatch": ["cfgd.yaml"],
        "url": "https://www.schemastore.org/cfgd-config-0.4.1.json",
        "versions": { "0.4.1": "https://www.schemastore.org/cfgd-config-0.4.1.json" },
    });
    let cat = catalog_with(&[prior]);
    let plan = plan_schema(
        &e,
        "cfgd machine configuration",
        true,
        Some("0.4.2"),
        Some(&cat),
    )
    .unwrap();
    let versions = plan
        .desired_entry
        .get("versions")
        .and_then(Value::as_object)
        .unwrap();
    assert!(
        versions.contains_key("0.4.1"),
        "prior version must carry forward across a name-case drift"
    );
    assert!(versions.contains_key("0.4.2"), "new version added");
}

// --- allowlist key derivation ---------------------------------------

#[test]
fn allowlist_key_is_vendor_filename_with_json_extension() {
    // The catalog display name here is the Title-case `cfgd-module`, but the
    // allowlist key must be the vendored file basename WITH `.json` so
    // SchemaStore's `path.basename` match succeeds — never the display name.
    let e = SchemaEntry {
        name: "cfgd-module".into(),
        file_match: vec!["cfgd.yaml".into()],
        schema_file: Some("schemas/cfgd-module.schema.json".into()),
        description: Some("cfgd module configuration".into()),
        ..Default::default()
    };
    let plan = plan_schema(&e, "cfgd module configuration", false, None, None).unwrap();
    assert_eq!(allowlist_name_for(&plan).unwrap(), "cfgd-module.json");
    assert_ne!(allowlist_name_for(&plan).unwrap(), "cfgd-module");
}

#[test]
fn allowlist_key_is_versioned_vendor_filename() {
    let e = SchemaEntry {
        name: "cfgd-module".into(),
        file_match: vec!["cfgd.yaml".into()],
        schema_file: Some("schemas/cfgd-module.schema.json".into()),
        description: Some("cfgd module configuration".into()),
        ..Default::default()
    };
    let plan = plan_schema(&e, "cfgd module configuration", true, Some("0.4.2"), None).unwrap();
    assert_eq!(allowlist_name_for(&plan).unwrap(), "cfgd-module-0.4.2.json");
}

// --- verdict against a fixture catalog ------------------------------

#[test]
fn plan_verdict_is_add_when_absent() {
    let e = external_entry();
    let cat = catalog_with(&[serde_json::json!({
        "name": "SomethingElse",
        "description": "other",
        "fileMatch": ["x.yaml"],
        "url": "https://example.com/x.json",
    })]);
    let plan = plan_schema(
        &e,
        "Anodizer Rust release-automation configuration file",
        false,
        None,
        Some(&cat),
    )
    .unwrap();
    assert_eq!(plan.verdict, Some(catalog::Verdict::Add));
}

#[test]
fn plan_verdict_is_noop_when_identical() {
    let e = external_entry();
    // The catalog already holds the exact desired entry.
    let desired = catalog::build_entry_json(
        &e.name,
        "Anodizer Rust release-automation configuration file",
        &e.file_match,
        e.url.as_deref().unwrap(),
        None,
    );
    let cat = catalog_with(&[desired]);
    let plan = plan_schema(
        &e,
        "Anodizer Rust release-automation configuration file",
        false,
        None,
        Some(&cat),
    )
    .unwrap();
    assert_eq!(plan.verdict, Some(catalog::Verdict::NoOp));
}

#[test]
fn plan_verdict_is_update_when_description_differs() {
    let e = external_entry();
    let stale = catalog::build_entry_json(
        &e.name,
        "an older description",
        &e.file_match,
        e.url.as_deref().unwrap(),
        None,
    );
    let cat = catalog_with(&[stale]);
    let plan = plan_schema(
        &e,
        "Anodizer Rust release-automation configuration file",
        false,
        None,
        Some(&cat),
    )
    .unwrap();
    assert_eq!(plan.verdict, Some(catalog::Verdict::Update));
}

// --- schema_change_needed (pure, shared by probe + run_real) --------
//
// This is the SINGLE change-decision both the pre-clone probe and
// `run_real` gate on. Its no-op result must be CERTAIN: every uncertainty
// (missing file, unfetched jsonc, malformed catalog) yields change-needed.

/// Build a vendor plan + its locally-formatted schema content, plus a
/// catalog that already holds the exact desired entry (so the catalog half
/// of the decision is a no-op and the file/dialect half is under test).
fn vendor_plan_with_matching_catalog(schema_src: &str) -> (SchemaPlan, String, String) {
    let e = vendor_entry();
    let plan = plan_schema(&e, "cfgd machine config", false, None, None).unwrap();
    let cat = catalog_with(std::slice::from_ref(&plan.desired_entry));
    let local = manifest::format_vendor_schema(schema_src).unwrap();
    (plan, cat, local)
}

/// A draft-07 schema body (dialect `Ok` ⇒ no allowlist needed).
const DRAFT07_SCHEMA: &str =
    r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"object"}"#;
/// A 2020-12 schema body (dialect `TooHigh` ⇒ allowlist required).
const DRAFT2020_SCHEMA: &str =
    r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#;

#[test]
fn change_needed_external_entry_match_is_noop() {
    let e = external_entry();
    let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
    let cat = catalog_with(std::slice::from_ref(&plan.desired_entry));
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: None,
        jsonc: None,
    };
    assert!(
        !schema_change_needed(&plan, None, &remote),
        "external entry matching the catalog must be a no-op"
    );
}

#[test]
fn change_needed_external_entry_absent_needs_change() {
    let e = external_entry();
    let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
    let cat = catalog_with(&[]); // entry absent ⇒ Add
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: None,
        jsonc: None,
    };
    assert!(schema_change_needed(&plan, None, &remote));
}

#[test]
fn change_needed_external_entry_differs_needs_change() {
    let e = external_entry();
    let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
    // Same name, different description ⇒ Update.
    let stale = catalog::build_entry_json(
        &e.name,
        "an older description",
        &e.file_match,
        e.url.as_deref().unwrap(),
        None,
    );
    let cat = catalog_with(&[stale]);
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: None,
        jsonc: None,
    };
    assert!(schema_change_needed(&plan, None, &remote));
}

#[test]
fn change_needed_vendor_entry_and_file_match_is_noop() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
    // Upstream file byte-equals the locally-formatted content.
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: Some(&local),
        jsonc: None,
    };
    assert!(
        !schema_change_needed(&plan, Some(&local), &remote),
        "vendor entry + file both matching (draft-07, no allowlist) ⇒ no-op"
    );
}

/// The latent-bug regression test: the catalog entry matches but the
/// vendored FILE content drifted. The old code gated on the catalog-entry
/// verdict alone (`catalog::verdict` ⇒ NoOp) and never re-pushed the file.
/// `schema_change_needed` compares the file too ⇒ change-needed.
#[test]
fn change_needed_vendor_file_drift_with_matching_catalog_needs_change() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
    // Prove the catalog half alone would have been a false no-op.
    assert_eq!(
        catalog::verdict(&cat, &plan.desired_entry).unwrap(),
        catalog::Verdict::NoOp,
        "precondition: catalog entry matches ⇒ entry-only verdict is NoOp"
    );
    // Upstream file content differs from the local formatted content.
    let drifted_upstream = manifest::format_vendor_schema(
        r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"string"}"#,
    )
    .unwrap();
    assert_ne!(local, drifted_upstream, "fixture must actually differ");
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: Some(&drifted_upstream),
        jsonc: None,
    };
    assert!(
        schema_change_needed(&plan, Some(&local), &remote),
        "vendor file content drift MUST trigger a change even when the \
             catalog entry is unchanged — this is the latent-bug regression"
    );
}

#[test]
fn change_needed_vendor_missing_upstream_file_is_conservative_change() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT07_SCHEMA);
    // Upstream file unfetched / absent ⇒ never a false no-op.
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: None,
        jsonc: None,
    };
    assert!(
        schema_change_needed(&plan, Some(&local), &remote),
        "a vendor schema with no upstream file content is uncertain ⇒ change"
    );
}

#[test]
fn change_needed_too_high_with_allowlisted_filename_is_noop() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
    let allow_name = allowlist_name_for(&plan).unwrap(); // cfgd-config.json
    let jsonc =
        format!("{{\n  // dialects\n  \"highSchemaVersion\": [\n    \"{allow_name}\"\n  ]\n}}\n");
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: Some(&local),
        jsonc: Some(&jsonc),
    };
    assert!(
        !schema_change_needed(&plan, Some(&local), &remote),
        "too-high vendor whose filename is already allowlisted ⇒ no-op"
    );
}

#[test]
fn change_needed_too_high_missing_from_allowlist_needs_change() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
    // Allowlist present but does NOT contain the vendored filename.
    let jsonc = "{\n  \"highSchemaVersion\": [\n    \"something-else.json\"\n  ]\n}\n";
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: Some(&local),
        jsonc: Some(jsonc),
    };
    assert!(
        schema_change_needed(&plan, Some(&local), &remote),
        "too-high vendor missing from the allowlist must be a change"
    );
}

#[test]
fn change_needed_too_high_unfetched_jsonc_is_conservative_change() {
    let (plan, cat, local) = vendor_plan_with_matching_catalog(DRAFT2020_SCHEMA);
    // Entry + file match, but the allowlist could not be fetched (None) ⇒
    // can't confirm the dialect is allowlisted ⇒ conservative change.
    let remote = RemoteState {
        catalog_json: &cat,
        vendor_file: Some(&local),
        jsonc: None,
    };
    assert!(
        schema_change_needed(&plan, Some(&local), &remote),
        "too-high vendor with unfetched jsonc is uncertain ⇒ change"
    );
}

#[test]
fn change_needed_malformed_catalog_is_conservative_change() {
    let e = external_entry();
    let plan = plan_schema(&e, "Anodizer config", false, None, None).unwrap();
    let remote = RemoteState {
        catalog_json: "{ not valid json",
        vendor_file: None,
        jsonc: None,
    };
    assert!(
        schema_change_needed(&plan, None, &remote),
        "a malformed catalog is uncertainty ⇒ change, never a false no-op"
    );
}

// --- dry-run run_publish (NO network) -------------------------------

#[test]
fn dry_run_external_logs_planned_line_and_opens_no_pr() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.with_log_capture(capture.clone());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![external_entry()],
        ..Default::default()
    };
    let ev = run_publish(&mut ctx).expect("dry-run external ok");
    assert_eq!(ev.publisher, "schemastore");
    assert_eq!(
        ev.extra,
        anodizer_core::PublishEvidenceExtra::Empty,
        "dry-run records no PR target"
    );
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("would") && m.contains("Anodizer") && m.contains("external")),
        "expected a planned 'would …' line naming the external schema; got {msgs:?}"
    );
}

#[test]
fn dry_run_vendor_logs_vendor_file_path_and_slug() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.with_log_capture(capture.clone());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![vendor_entry()],
        ..Default::default()
    };
    let ev = run_publish(&mut ctx).expect("dry-run vendor ok");
    assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter().any(|m| m.contains("would")
            && m.contains("cfgd-config")
            && m.contains("src/schemas/json/cfgd-config.json")),
        "expected a planned 'would …' line naming the vendor file path + slug; got {msgs:?}"
    );
}

#[test]
fn dry_run_skips_disabled_entries() {
    use anodizer_core::config::StringOrBool;
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.with_log_capture(capture.clone());
    let mut entry = external_entry();
    entry.skip = Some(StringOrBool::Bool(true));
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let ev = run_publish(&mut ctx).expect("dry-run skip ok");
    assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        !msgs.iter().any(|m| m.contains("would")),
        "a skipped entry must not produce a planned line; got {msgs:?}"
    );
}

#[test]
fn dry_run_if_false_filters_entry() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.with_log_capture(capture.clone());
    let mut entry = external_entry();
    // A falsy `if:` must filter the entry out of the effective set, the
    // same as `skip:` — exercising the `resolved_if` falsy branch.
    entry.if_condition = Some("false".into());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let ev = run_publish(&mut ctx).expect("dry-run if-false ok");
    assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        !msgs.iter().any(|m| m.contains("would")),
        "an `if: false` entry must not produce a planned line; got {msgs:?}"
    );
}

#[test]
fn empty_effective_set_returns_empty_evidence_and_logs_no_schemas() {
    let capture = anodizer_core::log::LogCapture::new();
    // Not dry-run: the early return must fire BEFORE any network/fork path,
    // proving the empty-set guard short-circuits regardless of mode.
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![],
        ..Default::default()
    };
    let ev = run_publish(&mut ctx).expect("empty schemas ok");
    assert_eq!(ev.publisher, "schemastore");
    assert_eq!(ev.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter().any(|m| m.contains("no schemas to register")),
        "expected the 'no schemas to register' status line; got {msgs:?}"
    );
}

// --- resolve_description (both branches) ----------------------------

#[test]
fn resolve_description_derives_from_project_metadata_when_unset() {
    let mut ctx = TestContextBuilder::new().build();
    ctx.config.metadata = Some(anodizer_core::config::MetadataConfig {
        description: Some("derived project config".into()),
        ..Default::default()
    });
    let mut entry = external_entry();
    entry.description = None; // force the metadata-derivation branch
    let desc = resolve_description(&ctx, &entry).expect("derive + sanitize ok");
    assert_eq!(desc, "derived project config");
}

#[test]
fn resolve_description_bails_when_nothing_derivable() {
    // No entry description and no project/crate metadata → the error path.
    let ctx = TestContextBuilder::new().build();
    let mut entry = external_entry();
    entry.description = None;
    let err =
        resolve_description(&ctx, &entry).expect_err("must bail when no description is derivable");
    let msg = err.to_string();
    assert!(
        msg.contains("Anodizer") && msg.contains("no description"),
        "expected an actionable no-description error; got {msg}"
    );
}

// --- PR body distinguishes vendor/versioned -------------------------

#[test]
fn pr_body_labels_external_vendor_and_versioned_distinctly() {
    let external = plan_schema(&external_entry(), "Anodizer config", false, None, None).unwrap();
    let vendor = plan_schema(&vendor_entry(), "cfgd machine config", false, None, None).unwrap();
    let versioned = plan_schema(
        &vendor_entry(),
        "cfgd machine config",
        true,
        Some("0.4.2"),
        None,
    )
    .unwrap();
    let body = schemastore_pr_body(&[external, vendor, versioned]);
    assert!(body.contains("**Anodizer** (external)"), "{body}");
    assert!(body.contains("**cfgd-config** (vendor) →"), "{body}");
    assert!(
            body.contains("**cfgd-config** (vendor, versioned) → https://www.schemastore.org/cfgd-config-0.4.2.json"),
            "versioned vendor must be labeled distinctly with its versioned url; got {body}"
        );
}

// --- per-crate version scope across config modes --------------------
//
// A versioned vendor schema stamps `<VER>` from the SCHEMA'S OWN crate's
// tag — never crate[0]'s — in every config mode. The all-config-modes axis
// is the canonical anodizer bug surface (flat/clobbering/first-crate
// resolution), so each mode gets an executable proof. `plan_schema_scoped`
// drives the real `with_published_crate_scope` → `resolve_crate_tag` path
// against a git fixture, so a regression of the scope to crate[0] would
// change the asserted `<VER>` and fail the test.

/// Build a versioned vendor entry bound to `crate_name`.
fn versioned_vendor_entry(crate_name: &str) -> SchemaEntry {
    SchemaEntry {
        name: "cfgd-config".into(),
        slug: Some("cfgd-config".into()),
        file_match: vec!["cfgd.yaml".into()],
        schema_file: Some("schemas/cfgd-config.schema.json".into()),
        crate_: Some(crate_name.into()),
        versioned: Some(true),
        description: Some("cfgd machine configuration".into()),
        ..Default::default()
    }
}

fn crate_cfg(name: &str, tag_template: &str) -> anodizer_core::config::CrateConfig {
    anodizer_core::config::CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(tag_template.to_string()),
        ..Default::default()
    }
}

/// The schema's crate (`cfgd`, tag `cfgd-v2.0.0` ⇒ 2.0.0) is the SECOND
/// crate; crate[0] is `cfgd-core` (tag `cfgd-core-v1.0.0` ⇒ 1.0.0). A
/// versioned vendor schema must stamp the bound crate's own version
/// (2.0.0) — if the scope regressed to crate[0], `<VER>` would be 1.0.0.
#[test]
fn per_crate_mode_stamps_schema_crate_version_not_crate_zero() {
    // Independent per-crate tags on a hermetic repo so the production
    // `resolve_crate_tag` path resolves each crate's OWN version.
    let repo = crate::testing::hermetic_repo_with_tags(&["cfgd-core-v1.0.0", "cfgd-v2.0.0"]);
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            crate_cfg("cfgd-core", "cfgd-core-v{{ .Version }}"),
            crate_cfg("cfgd", "cfgd-v{{ .Version }}"),
        ])
        .project_root(repo.path().to_path_buf())
        .build();

    let cfg = SchemastoreConfig::default();
    let entry = versioned_vendor_entry("cfgd");
    let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "cfgd machine configuration", None)
        .expect("plan_schema_scoped for per-crate versioned vendor");

    assert_eq!(
        plan.url, "https://www.schemastore.org/cfgd-config-2.0.0.json",
        "expected cfgd's own version 2.0.0 in the catalog url; \
             a scope regressed to crate[0] would yield cfgd-core's 1.0.0"
    );
    assert_eq!(
        plan.vendor_path.as_ref().unwrap(),
        &PathBuf::from("src/schemas/json/cfgd-config-2.0.0.json"),
        "expected the vendor filename stamped with cfgd's own 2.0.0"
    );
    assert!(
        !plan.url.contains("1.0.0"),
        "the schema's crate version (cfgd@2.0.0) must NOT be crate[0]'s \
             (cfgd-core@1.0.0); url was {}",
        plan.url
    );
    let versions = plan
        .desired_entry
        .get("versions")
        .and_then(Value::as_object)
        .expect("versioned entry carries a versions map");
    assert!(
        versions.contains_key("2.0.0"),
        "versions map keyed by cfgd's own 2.0.0; got {versions:?}"
    );
    assert!(
        !versions.contains_key("1.0.0"),
        "versions map must NOT carry crate[0]'s 1.0.0; got {versions:?}"
    );
}

/// Single-crate mode: one crate `mytool` tagged `v3.1.0`. A versioned
/// vendor schema (crate unset ⇒ defaults to the sole crate) stamps 3.1.0.
#[test]
fn single_crate_mode_stamps_sole_crate_version() {
    let repo = crate::testing::hermetic_repo_with_tags(&["v3.1.0"]);
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg("mytool", "v{{ .Version }}")])
        .project_root(repo.path().to_path_buf())
        .build();

    let cfg = SchemastoreConfig::default();
    // `crate` unset: `plan_schema_scoped` binds the version to the sole
    // crate via the all_crates().first() fallback.
    let mut entry = versioned_vendor_entry("mytool");
    entry.crate_ = None;
    let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "mytool configuration", None)
        .expect("plan_schema_scoped for single-crate versioned vendor");

    assert_eq!(
        plan.url, "https://www.schemastore.org/cfgd-config-3.1.0.json",
        "single-crate versioned vendor must stamp the sole crate's 3.1.0"
    );
    assert_eq!(
        plan.vendor_path.as_ref().unwrap(),
        &PathBuf::from("src/schemas/json/cfgd-config-3.1.0.json"),
    );
}

/// Lockstep mode: two crates share ONE tag `v4.0.0`. A versioned schema
/// bound to the SECOND crate must still resolve the shared 4.0.0,
/// proving lockstep resolution is independent of which crate is named.
#[test]
fn lockstep_mode_stamps_shared_version_regardless_of_named_crate() {
    // Both crates use the same `v{{ .Version }}` template, so the single
    // `v4.0.0` tag resolves identically for either crate.
    let repo = crate::testing::hermetic_repo_with_tags(&["v4.0.0"]);
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            crate_cfg("alpha", "v{{ .Version }}"),
            crate_cfg("beta", "v{{ .Version }}"),
        ])
        .project_root(repo.path().to_path_buf())
        .build();

    let cfg = SchemastoreConfig::default();
    let entry = versioned_vendor_entry("beta");
    let plan = plan_schema_scoped(&mut ctx, &cfg, &entry, "beta configuration", None)
        .expect("plan_schema_scoped for lockstep versioned vendor");

    assert_eq!(
        plan.url, "https://www.schemastore.org/cfgd-config-4.0.0.json",
        "lockstep versioned vendor must stamp the shared 4.0.0 even for \
             the second-named crate"
    );
    assert_eq!(
        plan.vendor_path.as_ref().unwrap(),
        &PathBuf::from("src/schemas/json/cfgd-config-4.0.0.json"),
    );
}

// --- evidence shape -------------------------------------------------

#[test]
fn schemastore_evidence_carries_pr_target_with_env_var_name_not_value() {
    let ev = schemastore_evidence("tj-smith47", "schemastore-v0.4.2");
    match ev.extra {
        anodizer_core::PublishEvidenceExtra::Schemastore(s) => {
            let t = &s.schemastore_targets[0];
            assert_eq!(t.upstream_owner, "SchemaStore");
            assert_eq!(t.upstream_repo, "schemastore");
            assert_eq!(t.fork_owner, "tj-smith47");
            assert_eq!(t.branch, "schemastore-v0.4.2");
            assert_eq!(t.token_env_var.as_deref(), Some("SCHEMASTORE_TOKEN"));
        }
        other => panic!("expected Schemastore extra, got {other:?}"),
    }
}

// ===================================================================
// I/O shell: the PUBLISH flow that touches git + the filesystem.
//
// These exercise the helpers `run_real` orchestrates — the file
// read/format/write seams, the upstream-sync git plumbing, and the
// pre-clone guards — against a local bare repo (no network). The
// pattern mirrors `util/pr.rs`: a `file://`-equivalent local git repo
// reached through `repository.git.url`, a failing `gh`/network surface
// forced by a non-resolvable upstream, and `StageLogger::with_capture`
// for status-line assertions. Fixture git commits carry a
// per-invocation identity (`git_test_ok`), so no test here needs
// serialization.
// ===================================================================

/// Quiet logger paired with an in-memory capture so the helper's status
/// lines can be asserted without a Context.
fn capturing_log() -> (StageLogger, anodizer_core::log::LogCapture) {
    StageLogger::with_capture("schemastore", anodizer_core::log::Verbosity::Quiet)
}

fn quiet_log() -> StageLogger {
    StageLogger::new("schemastore", anodizer_core::log::Verbosity::Quiet)
}

// --- read_local_vendor_schema --------------------------------------

#[test]
fn read_local_vendor_schema_reads_and_reformats_from_project_root() {
    // A minified source schema under project_root must come back
    // reformatted to SchemaStore's prettier defaults (2-space indent,
    // trailing newline) — the exact bytes a publish would write.
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir_all(root.path().join("schemas")).unwrap();
    let minified = r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"object"}"#;
    std::fs::write(
        root.path().join("schemas/cfgd-config.schema.json"),
        minified,
    )
    .unwrap();

    let formatted = read_local_vendor_schema(root.path(), &vendor_entry())
        .expect("read + reformat vendor schema");
    // Reformatted: pretty-printed, distinct from the minified source.
    assert_ne!(formatted, minified, "must be reformatted, not echoed");
    assert!(
        formatted.ends_with("}\n"),
        "SchemaStore formatting appends a trailing newline; got {formatted:?}"
    );
    assert!(
        formatted.contains("\n  \"type\""),
        "expected 2-space indented pretty output; got {formatted:?}"
    );
    // Byte-identical to the shared formatter, so the change-decision and
    // the write path derive the same content.
    assert_eq!(formatted, manifest::format_vendor_schema(minified).unwrap());
}

#[test]
fn read_local_vendor_schema_errors_when_schema_file_unset() {
    // An entry with no `schema_file` cannot be vendored — the error
    // must name the offending entry.
    let root = tempfile::tempdir().expect("root");
    let mut entry = vendor_entry();
    entry.schema_file = None;
    let err =
        read_local_vendor_schema(root.path(), &entry).expect_err("missing schema_file must error");
    let msg = err.to_string();
    assert!(
        msg.contains("cfgd-config") && msg.contains("no schema_file"),
        "expected an actionable no-schema_file error naming the entry; got {msg}"
    );
}

#[test]
fn read_local_vendor_schema_errors_when_file_absent_on_disk() {
    // `schema_file` set but the file is missing under project_root.
    let root = tempfile::tempdir().expect("root");
    let err =
        read_local_vendor_schema(root.path(), &vendor_entry()).expect_err("absent file must error");
    assert!(
        err.to_string().contains("read schema_file"),
        "expected a read-failure context; got {err}"
    );
}

// --- read_cloned_vendor_file / read_cloned_jsonc -------------------

#[test]
fn read_cloned_vendor_file_returns_content_when_present_else_none() {
    let repo = tempfile::tempdir().expect("repo");
    let plan = plan_schema(&vendor_entry(), "cfgd machine config", false, None, None).unwrap();
    // Absent upstream copy ⇒ None (which the decision reads as change-needed).
    assert!(
        read_cloned_vendor_file(repo.path(), &plan).is_none(),
        "absent cloned vendor file must yield None"
    );
    // Seed the cloned tree at the plan's vendor path.
    let rel = plan.vendor_path.as_ref().unwrap();
    std::fs::create_dir_all(repo.path().join(rel).parent().unwrap()).unwrap();
    std::fs::write(repo.path().join(rel), "UPSTREAM-COPY\n").unwrap();
    assert_eq!(
        read_cloned_vendor_file(repo.path(), &plan).as_deref(),
        Some("UPSTREAM-COPY\n"),
        "present cloned vendor file must be returned verbatim"
    );
}

#[test]
fn read_cloned_vendor_file_none_for_external_plan_without_path() {
    // An external plan has no vendor_path, so the read is short-circuited
    // to None without touching the filesystem.
    let repo = tempfile::tempdir().expect("repo");
    let plan = plan_schema(&external_entry(), "Anodizer config", false, None, None).unwrap();
    assert!(read_cloned_vendor_file(repo.path(), &plan).is_none());
}

#[test]
fn read_cloned_jsonc_returns_allowlist_when_present_else_none() {
    let repo = tempfile::tempdir().expect("repo");
    assert!(
        read_cloned_jsonc(repo.path()).is_none(),
        "absent schema-validation.jsonc must yield None"
    );
    let abs = repo.path().join(DIALECT_ALLOWLIST_PATH);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, "{ \"highSchemaVersion\": [] }\n").unwrap();
    assert_eq!(
        read_cloned_jsonc(repo.path()).as_deref(),
        Some("{ \"highSchemaVersion\": [] }\n")
    );
}

// --- write_vendor_schema -------------------------------------------

#[test]
fn write_vendor_schema_creates_parent_dirs_and_writes_formatted_bytes() {
    // The cloned repo has no `src/schemas/json/` dir yet — the writer
    // must mkdir -p the parents and land the formatted bytes there.
    let repo = tempfile::tempdir().expect("repo");
    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let formatted = manifest::format_vendor_schema(DRAFT07_SCHEMA).expect("format draft-07 schema");
    let (log, cap) = capturing_log();

    write_vendor_schema(repo.path(), &entry, &plan, &formatted, &log)
        .expect("write vendor schema into a fresh clone");

    let dest = repo.path().join("src/schemas/json/cfgd-config.json");
    assert!(dest.exists(), "vendored file must be created at {dest:?}");
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        formatted,
        "the written bytes must byte-equal the formatted schema"
    );
    let msgs: Vec<String> = cap.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("vendored") && m.contains("src/schemas/json/cfgd-config.json")),
        "expected a 'vendored … → path' status line; got {msgs:?}"
    );
}

#[test]
fn write_vendor_schema_allowlists_too_high_dialect_in_same_pr() {
    // A 2020-12 schema must (a) be written AND (b) have its vendored
    // filename appended to `highSchemaVersion` in the SAME clone, so
    // SchemaStore CI accepts the high dialect. Seed an empty allowlist
    // so we can observe the append.
    let repo = tempfile::tempdir().expect("repo");
    let allow_abs = repo.path().join(DIALECT_ALLOWLIST_PATH);
    std::fs::create_dir_all(allow_abs.parent().unwrap()).unwrap();
    std::fs::write(&allow_abs, "{\n  \"highSchemaVersion\": []\n}\n").unwrap();

    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let formatted =
        manifest::format_vendor_schema(DRAFT2020_SCHEMA).expect("format 2020-12 schema");
    let (log, cap) = capturing_log();

    write_vendor_schema(repo.path(), &entry, &plan, &formatted, &log)
        .expect("write too-high vendor schema");

    // The allowlist now contains the vendored filename WITH `.json`.
    let updated = std::fs::read_to_string(&allow_abs).unwrap();
    assert!(
        super::super::scan::jsonc_array_contains(&updated, "highSchemaVersion", "cfgd-config.json"),
        "too-high dialect must allowlist its vendored basename; got {updated}"
    );
    let msgs: Vec<String> = cap.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("allowlisted") && m.contains("cfgd-config.json")),
        "expected an 'allowlisted … cfgd-config.json' status line; got {msgs:?}"
    );
}

#[test]
fn write_vendor_schema_ok_dialect_does_not_touch_allowlist() {
    // A draft-07 schema is accepted unconditionally — the allowlist must
    // be left exactly as found (no spurious append).
    let repo = tempfile::tempdir().expect("repo");
    let allow_abs = repo.path().join(DIALECT_ALLOWLIST_PATH);
    std::fs::create_dir_all(allow_abs.parent().unwrap()).unwrap();
    let original = "{\n  \"highSchemaVersion\": []\n}\n";
    std::fs::write(&allow_abs, original).unwrap();

    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let formatted = manifest::format_vendor_schema(DRAFT07_SCHEMA).unwrap();

    write_vendor_schema(repo.path(), &entry, &plan, &formatted, &quiet_log())
        .expect("write ok-dialect vendor schema");

    assert_eq!(
        std::fs::read_to_string(&allow_abs).unwrap(),
        original,
        "an OK dialect must not mutate the allowlist"
    );
}

#[test]
fn write_vendor_schema_errors_when_plan_has_no_path() {
    // An external plan reaching the writer is a bug; the writer guards
    // it with an explicit error rather than panicking on the None path.
    let repo = tempfile::tempdir().expect("repo");
    let entry = external_entry();
    let plan = plan_schema(&entry, "Anodizer config", false, None, None).unwrap();
    let err = write_vendor_schema(repo.path(), &entry, &plan, "{}\n", &quiet_log())
        .expect_err("external plan has no vendor path");
    assert!(
        err.to_string().contains("no path"),
        "expected a 'no path' error; got {err}"
    );
}

// --- raw_dialect (classifier off raw bytes) ------------------------

#[test]
fn raw_dialect_classifies_too_high_ok_and_unknown_from_schema_field() {
    assert_eq!(raw_dialect(DRAFT2020_SCHEMA), Dialect::TooHigh);
    assert_eq!(raw_dialect(DRAFT07_SCHEMA), Dialect::Ok);
    // No `$schema` field ⇒ Unknown (caller skips the allowlist).
    assert_eq!(raw_dialect(r#"{"type":"object"}"#), Dialect::Unknown);
    // Malformed JSON also degrades to Unknown rather than panicking.
    assert_eq!(raw_dialect("not json at all"), Dialect::Unknown);
}

// --- sync_to_upstream (git plumbing) -------------------------------
//
// Real contract: `git remote add upstream` is best-effort (`let _ =`,
// tolerating "already exists"), but the `git fetch` and `git reset
// --hard` steps are `?`-propagated. The fetch targets the hardcoded
// PUBLIC `github.com/SchemaStore/schemastore.git`, which a networked
// runner reaches WITHOUT auth — so against a real cloned fork the
// function SUCCEEDS (it does not error on "unreachable"; the upstream
// is reachable). The genuinely-erroring, hermetic path is a working
// dir that is not a git repo at all: `git fetch` then fails locally
// (no `.git`) before any network contact, and the error propagates.

#[test]
fn sync_to_upstream_propagates_fetch_error_in_a_non_repo_dir() {
    // A dir with no `.git`: `git remote add upstream` fails (ignored,
    // best-effort), then `git fetch --depth=1 upstream master` fails
    // because the cwd is not a repository — purely local, no network —
    // and the `?` surfaces it so `run_real` aborts before splicing.
    let not_a_repo = tempfile::tempdir().expect("scratch dir");

    let err = sync_to_upstream(not_a_repo.path(), &quiet_log())
        .expect_err("git fetch in a non-repo dir must surface as an error");
    assert!(
        err.to_string().contains("git fetch upstream"),
        "expected the fetch-upstream failure context (the `?`-propagated \
             step); got {err}"
    );
}

// --- run_real pre-clone guards (no network reached) ----------------

/// A schemastore config whose single effective schema is `external_entry`,
/// optionally carrying a `repository`. Drives `run_real` through its
/// guard ladder without ever reaching the network probe (which only
/// `run_publish`'s non-dry-run path hits).
fn schemastore_cfg(repo: Option<RepositoryConfig>) -> SchemastoreConfig {
    SchemastoreConfig {
        schemas: vec![external_entry()],
        repository: repo,
        ..Default::default()
    }
}

#[test]
fn run_real_bails_on_empty_version_before_any_clone() {
    // Version is the branch key (`schemastore-v<version>`); an empty one
    // would collide release-to-release and defeat the duplicate-PR guard.
    // `run_real` must bail BEFORE the irreversible clone/push.
    let mut ctx = TestContextBuilder::new().populate_git_vars(false).build();
    assert!(ctx.version().is_empty(), "precondition: empty Version");
    let cfg = schemastore_cfg(Some(RepositoryConfig {
        owner: Some("tj-smith47".into()),
        name: Some("schemastore".into()),
        ..Default::default()
    }));
    let effective = vec![(&cfg.schemas[0], "Anodizer config".to_string())];
    let err = run_real(&mut ctx, &cfg, &effective, &quiet_log())
        .expect_err("empty Version must abort run_real");
    assert!(
        err.to_string().contains("Version is empty"),
        "expected the empty-Version guard; got {err}"
    );
}

#[test]
fn run_real_bails_when_no_repository_fork_configured() {
    // No `repository` ⇒ no fork to push the branch / open the PR.
    let mut ctx = TestContextBuilder::new().build();
    assert!(!ctx.version().is_empty(), "precondition: non-empty Version");
    let cfg = schemastore_cfg(None);
    let effective = vec![(&cfg.schemas[0], "Anodizer config".to_string())];
    let err = run_real(&mut ctx, &cfg, &effective, &quiet_log())
        .expect_err("missing repository must abort run_real");
    let msg = err.to_string();
    assert!(
        msg.contains("no `repository`") && msg.contains("fork is required"),
        "expected the no-fork guard; got {msg}"
    );
}

#[test]
fn run_real_bails_when_repository_missing_owner_or_name() {
    // A `repository` present but missing owner/name cannot resolve a
    // clone target — the guard fires before the clone.
    let mut ctx = TestContextBuilder::new().build();
    let cfg = schemastore_cfg(Some(RepositoryConfig {
        owner: Some("tj-smith47".into()),
        name: None, // missing name
        ..Default::default()
    }));
    let effective = vec![(&cfg.schemas[0], "Anodizer config".to_string())];
    let err = run_real(&mut ctx, &cfg, &effective, &quiet_log())
        .expect_err("missing repository name must abort run_real");
    assert!(
        err.to_string().contains("must set both `owner` and `name`"),
        "expected the owner/name guard; got {err}"
    );
}

#[test]
fn run_real_aborts_when_fork_clone_fails() {
    // Past the pre-clone guards, the first irreversible step is cloning
    // the fork. A `repository.git.url` pointing at a path that is not a
    // git repo makes `clone_repo` (SSH/local-path branch) fail — purely
    // local, no network — so `run_real` aborts at the clone, BEFORE the
    // live upstream sync / any splice / push / PR. This proves the clone
    // seam's error gates the rest of the flow without depending on the
    // (reachable, public) SchemaStore upstream or a push to a fixture.
    let bogus = tempfile::tempdir().expect("scratch");
    let bogus_url = bogus
        .path()
        .join("not-a-repo")
        .to_string_lossy()
        .into_owned();

    let mut ctx = TestContextBuilder::new().build();
    assert!(!ctx.version().is_empty(), "precondition: non-empty Version");
    let cfg = schemastore_cfg(Some(RepositoryConfig {
        owner: Some("tj-smith47".into()),
        name: Some("schemastore".into()),
        // Local-path "git url" that is not a repository ⇒ clone fails.
        git: Some(anodizer_core::config::GitRepoConfig {
            url: Some(bogus_url),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let effective = vec![(&cfg.schemas[0], "Anodizer config".to_string())];
    let err = run_real(&mut ctx, &cfg, &effective, &quiet_log())
        .expect_err("a failed fork clone must abort run_real");
    // The clone helper labels its failures with the publisher name; the
    // abort must originate at the clone, not later in the flow.
    assert!(
        err.to_string().contains("schemastore"),
        "expected the clone-failure error from the `schemastore` clone \
             step; got {err}"
    );
}

// --- commit / PR text builders -------------------------------------

#[test]
fn schemastore_branch_keys_on_version() {
    assert_eq!(schemastore_branch("0.4.2"), "schemastore-v0.4.2");
    assert_eq!(schemastore_branch("1.0.0-rc.1"), "schemastore-v1.0.0-rc.1");
}

#[test]
fn commit_msg_and_pr_title_name_every_applied_schema() {
    // Plans built with `None` catalog carry `verdict: None`, which the
    // summary treats as an add — so both surfaces read "Add ...".
    let a = plan_schema(&external_entry(), "Anodizer config", false, None, None).unwrap();
    let b = plan_schema(&vendor_entry(), "cfgd machine config", false, None, None).unwrap();
    let applied = vec![a, b];
    assert_eq!(
        schemastore_commit_msg(&applied),
        "Add Anodizer, cfgd-config schema(s)"
    );
    assert_eq!(
        schemastore_pr_title(&applied),
        "Add Anodizer, cfgd-config schema(s)"
    );
}

/// The PR title/commit verb is derived per-plan from its [`catalog::Verdict`]:
/// all-Add ⇒ "Add", all-Update ⇒ "Update", mixed ⇒ "Add a; update b". This
/// is the user's contract — "Add if it doesn't exist, update if it does."
/// A `NoOp` plan (catalog entry unchanged but vendored file refreshed; it
/// still reaches `applied` via the file-diff gate) groups under "Update".
#[test]
fn summary_derives_verb_from_each_plans_verdict() {
    let plan = |name: &str, verdict| SchemaPlan {
        name: name.into(),
        mode: SchemaMode::External,
        url: "https://x/s.json".into(),
        vendor_path: None,
        versioned: false,
        desired_entry: serde_json::json!({}),
        verdict: Some(verdict),
    };
    assert_eq!(
        schemastore_summary(&[plan("Aaa", catalog::Verdict::Add)]),
        "Add Aaa"
    );
    assert_eq!(
        schemastore_summary(&[plan("Bbb", catalog::Verdict::Update)]),
        "Update Bbb"
    );
    // A NoOp plan that reached `applied` is a vendor file refresh ⇒ "Update".
    assert_eq!(
        schemastore_summary(&[plan("Ccc", catalog::Verdict::NoOp)]),
        "Update Ccc"
    );
    assert_eq!(
        schemastore_summary(&[
            plan("Aaa", catalog::Verdict::Add),
            plan("Bbb", catalog::Verdict::Update),
            plan("Ccc", catalog::Verdict::NoOp),
        ]),
        "Add Aaa; update Bbb, Ccc"
    );
}

// --- planned_line: every verdict arm renders a distinct verb ---------
//
// The dry-run path leaves `verdict: None` ("register/refresh"); the
// real path resolves Add/Update/NoOp. `planned_line` must map each to
// its own operator-facing verb so the log never claims "register" for
// a no-op (the verbs differ from the PR-title/commit text on purpose).

/// Build an external plan against a fixture catalog so its `verdict`
/// resolves to `want`, then return the rendered `planned_line`.
fn planned_line_for_verdict(want: &catalog::Verdict) -> String {
    let e = external_entry();
    let desired = "Anodizer Rust release-automation configuration file";
    let cat = match want {
        // Entry absent ⇒ Add.
        catalog::Verdict::Add => catalog_with(&[]),
        // Entry present, identical ⇒ NoOp.
        catalog::Verdict::NoOp => {
            let same = catalog::build_entry_json(
                &e.name,
                desired,
                &e.file_match,
                e.url.as_deref().unwrap(),
                None,
            );
            catalog_with(&[same])
        }
        // Entry present, stale description ⇒ Update.
        catalog::Verdict::Update => {
            let stale = catalog::build_entry_json(
                &e.name,
                "an older description",
                &e.file_match,
                e.url.as_deref().unwrap(),
                None,
            );
            catalog_with(&[stale])
        }
    };
    let plan = plan_schema(&e, desired, false, None, Some(&cat)).unwrap();
    assert_eq!(
        plan.verdict.as_ref(),
        Some(want),
        "fixture must produce {want:?}"
    );
    plan.planned_line()
}

#[test]
fn planned_line_verbs_match_each_resolved_verdict() {
    // No-op: the operator must see "already registered", never "register".
    let noop = planned_line_for_verdict(&catalog::Verdict::NoOp);
    assert!(
        noop.contains("would no-op (already registered) `Anodizer`"),
        "no-op verdict must render the no-op verb; got {noop}"
    );
    // Add: "register".
    let add = planned_line_for_verdict(&catalog::Verdict::Add);
    assert!(
        add.contains("would register `Anodizer`") && !add.contains("no-op"),
        "Add verdict must render the register verb; got {add}"
    );
    // Update: "refresh".
    let update = planned_line_for_verdict(&catalog::Verdict::Update);
    assert!(
        update.contains("would refresh `Anodizer`") && !update.contains("no-op"),
        "Update verdict must render the refresh verb; got {update}"
    );
}

#[test]
fn planned_line_vendor_appends_versioned_file_path() {
    // A versioned vendor plan (verdict left unresolved) renders the
    // "vendor, versioned" mode label AND the versioned vendor file path.
    let plan = plan_schema(
        &vendor_entry(),
        "cfgd machine config",
        true,
        Some("0.4.2"),
        None,
    )
    .unwrap();
    let line = plan.planned_line();
    assert!(
        line.contains("would register/refresh `cfgd-config` (vendor, versioned)"),
        "unresolved verdict + versioned vendor mode label; got {line}"
    );
    assert!(
        line.contains("url https://www.schemastore.org/cfgd-config-0.4.2.json"),
        "versioned url in the planned line; got {line}"
    );
    assert!(
        line.contains(", vendor file src/schemas/json/cfgd-config-0.4.2.json"),
        "versioned vendor file path appended; got {line}"
    );
}

// --- plan_schema error arm: versioned vendor needs a version ---------

#[test]
fn plan_versioned_vendor_without_version_errors() {
    // A versioned vendor entry needs a resolved crate version to stamp
    // `<VER>`; passing `version: None` must surface an actionable error
    // naming the entry, never silently emit a bare `-.json` file.
    let err = plan_schema(&vendor_entry(), "cfgd machine config", true, None, None)
        .expect_err("versioned vendor without a version must error");
    let msg = err.to_string();
    assert!(
        msg.contains("cfgd-config") && msg.contains("needs a resolved crate version"),
        "expected the missing-version error naming the entry; got {msg}"
    );
}

// --- upstream_versions: malformed catalog surfaces an Err ------------

#[test]
fn upstream_versions_returns_err_on_malformed_catalog() {
    // A versioned vendor plan reads prior versions off the upstream
    // catalog; a malformed catalog must surface as `Some(Err)` (which
    // `plan_schema` `?`-propagates) rather than silently dropping the
    // carry-forward and orphaning older versioned files.
    let got = catalog::upstream_versions_by_file_match("{ not json", &["cfgd.yaml".to_string()]);
    match got {
        Some(Err(_)) => {}
        other => panic!("malformed catalog must yield Some(Err); got {other:?}"),
    }
}

#[test]
fn upstream_versions_none_when_entry_absent_or_unversioned() {
    let fm = ["cfgd.yaml".to_string()];
    // Entry absent ⇒ None (no prior versions to carry).
    let empty = catalog_with(&[]);
    assert!(
        catalog::upstream_versions_by_file_match(&empty, &fm).is_none(),
        "absent entry must yield None, not an error"
    );
    // Entry present but with no `versions` map ⇒ None.
    let no_versions = catalog_with(&[serde_json::json!({
        "name": "cfgd-config",
        "description": "x",
        "fileMatch": ["cfgd.yaml"],
        "url": "https://www.schemastore.org/cfgd-config.json",
    })]);
    assert!(
        catalog::upstream_versions_by_file_match(&no_versions, &fm).is_none(),
        "an entry without a versions map must yield None"
    );
}

#[test]
fn plan_versioned_vendor_carry_forward_propagates_malformed_catalog() {
    // The `?` on `upstream_versions(...).transpose()` inside the
    // versioned-vendor arm must propagate a malformed-catalog error out
    // of `plan_schema` — a corrupt upstream catalog can't silently
    // collapse the versions carry-forward.
    let err = plan_schema(
        &vendor_entry(),
        "cfgd machine config",
        true,
        Some("0.4.2"),
        Some("{ not valid json"),
    )
    .expect_err("malformed carry-forward catalog must error");
    // serde's parse error message; the point is the `?` fired.
    assert!(
        err.to_string().contains("expected")
            || err.to_string().contains("key")
            || err.to_string().contains("value"),
        "expected a JSON-parse error from the carry-forward read; got {err}"
    );
}

// --- allowlist_name_for: no-vendor-path arm --------------------------

#[test]
fn allowlist_name_for_errors_for_external_plan_without_path() {
    // An external plan carries no vendor path, so deriving an allowlist
    // key is a logic error — the function must say so, naming the entry,
    // rather than unwrap-panicking on the `None` path.
    let plan = plan_schema(&external_entry(), "Anodizer config", false, None, None).unwrap();
    assert!(plan.vendor_path.is_none(), "precondition: no vendor path");
    let err =
        allowlist_name_for(&plan).expect_err("external plan has no path for the allowlist key");
    assert!(
        err.to_string().contains("Anodizer") && err.to_string().contains("no path"),
        "expected a 'no path' error naming the entry; got {err}"
    );
}

#[test]
fn allowlist_name_for_errors_when_vendor_path_has_no_file_name() {
    // A bare root path ("/") is `Some` but has no basename, so `file_name()`
    // returns `None`. The function must surface a "no file name" error naming
    // the entry rather than unwrap-panicking on the missing basename.
    let plan = SchemaPlan {
        name: "Rooty".into(),
        mode: SchemaMode::Vendor,
        url: "https://www.schemastore.org/rooty.json".into(),
        vendor_path: Some(PathBuf::from("/")),
        versioned: false,
        desired_entry: serde_json::json!({}),
        verdict: None,
    };
    let err = allowlist_name_for(&plan).expect_err("root path has no file name");
    assert!(
        err.to_string().contains("Rooty") && err.to_string().contains("no file name"),
        "expected a 'no file name' error naming the entry; got {err}"
    );
}

// --- resolve_description: crate-bound derivation branch --------------

#[test]
fn resolve_description_derives_from_bound_crate_metadata() {
    // An entry with `crate: <name>` and no inline description derives
    // from THAT crate's metadata via `meta_description_for` — the
    // crate-scoped branch, distinct from the project-metadata branch.
    let mut ctx = TestContextBuilder::new().build();
    // Top-level metadata.description wins inside `meta_description_for`,
    // so set it and bind the entry to a crate to exercise the
    // `crate_.as_deref() => Some` arm.
    ctx.config.metadata = Some(anodizer_core::config::MetadataConfig {
        description: Some("crate-derived description".into()),
        ..Default::default()
    });
    let mut entry = vendor_entry();
    entry.description = None;
    entry.crate_ = Some("cfgd".into());
    let desc = resolve_description(&ctx, &entry).expect("derive from bound crate metadata");
    assert_eq!(desc, "crate-derived description");
}

// --- effective_schemas: a malformed `if:` propagates -----------------

#[test]
fn run_publish_propagates_malformed_if_render_error() {
    // A malformed `if:` template (unterminated Tera) must surface as an
    // Err out of the effective-set resolution — never silently keep or
    // drop the entry. Dry-run so the failure is reached before any
    // network/clone path.
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    let mut entry = external_entry();
    entry.if_condition = Some("{{ unterminated".into());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let err = run_publish(&mut ctx).expect_err("malformed `if:` must surface as Err");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("if") || chain.contains("template") || chain.contains("render"),
        "expected an if-template render error in the chain; got {chain}"
    );
}

// --- effective_schemas: cross-leg crate filtering ---------------------
//
// Per-crate / workspace-split publish runs each leg with a `ctx.config`
// whose crate universe holds only that leg's crates. A schema entry bound
// to a crate outside the current leg's universe belongs to another leg
// and must be filtered before `resolve_description` ever sees it.

#[test]
fn effective_schemas_filters_entry_bound_to_crate_outside_leg_universe() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg("cfgd-core", "v{{ .Version }}")])
        .build();
    let mut entry = vendor_entry();
    entry.crate_ = Some("cfgd".into());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let effective = effective_schemas(&ctx, &ctx.config.schemastore, &quiet_log())
        .expect("cross-leg entry must be filtered, not errored");
    assert!(
        effective.is_empty(),
        "an entry bound to a crate absent from this leg's universe must be dropped; got {effective:?}"
    );
}

#[test]
fn effective_schemas_keeps_entry_bound_to_crate_in_leg_universe() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg("cfgd", "v{{ .Version }}")])
        .build();
    let mut entry = vendor_entry();
    entry.crate_ = Some("cfgd".into());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let effective = effective_schemas(&ctx, &ctx.config.schemastore, &quiet_log())
        .expect("owning-leg entry must resolve");
    assert_eq!(
        effective.len(),
        1,
        "an entry whose bound crate IS in the universe must be kept"
    );
}

#[test]
fn effective_schemas_keeps_project_bound_entry_regardless_of_universe() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg("cfgd-core", "v{{ .Version }}")])
        .build();
    let mut entry = vendor_entry();
    entry.crate_ = None;
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let effective = effective_schemas(&ctx, &ctx.config.schemastore, &quiet_log())
        .expect("project-bound entry must resolve");
    assert_eq!(
        effective.len(),
        1,
        "an entry with crate_ == None is project/primary-bound and present on every leg — \
             must never be filtered"
    );
}

#[test]
fn effective_schemas_all_cross_leg_yields_empty() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg("cfgd-core", "v{{ .Version }}")])
        .build();
    let mut entry = vendor_entry();
    entry.crate_ = Some("cfgd".into());
    ctx.config.schemastore = SchemastoreConfig {
        schemas: vec![entry],
        ..Default::default()
    };
    let effective = effective_schemas(&ctx, &ctx.config.schemastore, &quiet_log())
        .expect("all-cross-leg config must resolve, not error");
    assert!(
        effective.is_empty(),
        "when every entry is cross-leg, effective_schemas must return empty — this is what \
             drives run_publish's clean 'no schemas to register' no-op"
    );
}

// --- plan_schema_scoped: versioned vendor with no crate to bind ------

#[test]
fn plan_schema_scoped_versioned_vendor_no_crate_errors() {
    // A versioned vendor entry with no `crate` AND an empty crate set
    // (so the `all_crates().first()` fallback yields nothing) cannot
    // resolve a version scope — the guard must error, naming the entry,
    // rather than stamping a bogus `<VER>`.
    let mut ctx = TestContextBuilder::new().crates(vec![]).build();
    let cfg = SchemastoreConfig::default();
    let mut entry = versioned_vendor_entry("cfgd");
    entry.crate_ = None; // no explicit crate, and no crates to fall back to
    let err = plan_schema_scoped(&mut ctx, &cfg, &entry, "cfgd machine config", None)
        .expect_err("versioned vendor with no bindable crate must error");
    assert!(
        err.to_string().contains("cfgd-config") && err.to_string().contains("needs a `crate`"),
        "expected the no-crate-to-bind error naming the entry; got {err}"
    );
}

// --- fetch_raw_required / fetch_raw_optional (HTTP-mock, local TCP) --
//
// These take a `client` + `url` directly, so a local scripted responder
// exercises the success / 404 / non-success / status-mapping branches
// without any subprocess, PATH stub, or env mutation — cross-platform
// safe and ungated. (The probe's caller uses a hardcoded
// raw.githubusercontent URL, but these leaf fetchers are url-injectable.)

use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};

fn http_client() -> reqwest::blocking::Client {
    anodizer_core::http::blocking_client(std::time::Duration::from_secs(5))
        .expect("blocking http client")
}

#[test]
fn fetch_raw_required_returns_body_on_200() {
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/catalog.json",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"schemas\":1}",
        times: Some(1),
    }]);
    let body = fetch_raw_required(&http_client(), &format!("http://{addr}/catalog.json"))
        .expect("200 must return the body");
    assert_eq!(body, "{\"schemas\":1}", "the exact response body verbatim");
}

#[test]
fn fetch_raw_required_errors_on_404_with_status_in_message() {
    // For a REQUIRED fetch, even a 404 is an error (the probe falls
    // through to the clone); the message must carry the URL + status.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/missing.json",
        response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        times: Some(1),
    }]);
    let url = format!("http://{addr}/missing.json");
    let err = fetch_raw_required(&http_client(), &url).expect_err("404 must error");
    let msg = err.to_string();
    assert!(
        msg.contains("404") && msg.contains(&url),
        "required-fetch error must name the URL + status; got {msg}"
    );
}

#[test]
fn fetch_raw_optional_maps_200_to_some_404_to_none() {
    // Success ⇒ Some(body); a 404 ⇒ None (absent upstream ⇒ the
    // change-decision reads None as change-needed).
    let (addr, _log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/present.json",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/absent.json",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
    ]);
    let client = http_client();
    assert_eq!(
        fetch_raw_optional(&client, &format!("http://{addr}/present.json")).unwrap(),
        Some("hello".to_string()),
        "200 ⇒ Some(body)"
    );
    assert_eq!(
        fetch_raw_optional(&client, &format!("http://{addr}/absent.json")).unwrap(),
        None,
        "404 ⇒ None (file absent upstream)"
    );
}

#[test]
fn fetch_raw_optional_errors_on_non_404_non_success() {
    // A 500 is neither "absent" (404 ⇒ None) nor success ⇒ the optional
    // fetch must ERROR so the probe falls through rather than guessing.
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/boom.json",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        times: Some(1),
    }]);
    let url = format!("http://{addr}/boom.json");
    let err = fetch_raw_optional(&http_client(), &url).expect_err("500 must error");
    assert!(
        err.to_string().contains("500"),
        "non-404 non-success must surface the status; got {err}"
    );
}

// --- run_real I/O shell: post-clone splice/write/idempotency ---------
//
// `run_real` clones the fork, then `sync_to_upstream` fetches the
// hardcoded PUBLIC github.com/SchemaStore upstream (network). The
// genuinely hermetic seam these gate is the change-decision short-
// circuit: when EVERY effective schema is already current against the
// cloned tree, `run_real` returns the "nothing to publish" evidence
// WITHOUT a push or a PR. We exercise the splice/write/change-decision
// helpers it orchestrates directly, since the network sync sits between
// the clone and those helpers.
//
// These build bare working trees + read the process env, so they are
// `#[cfg(unix)]`-gated: the on-disk git plumbing + the unix-path / mode
// assumptions are POSIX-only, and gating costs zero coverage (coverage
// is measured on ubuntu).

/// A draft-07 vendor schema written under a fresh `project_root` so a
/// vendor plan's local read+format succeeds, plus the formatted bytes.
#[cfg(unix)]
fn seed_local_vendor_schema(entry: &SchemaEntry) -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().expect("project root");
    let rel = entry
        .schema_file
        .as_deref()
        .expect("vendor entry schema_file");
    let src = root.path().join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, DRAFT07_SCHEMA).unwrap();
    let formatted = manifest::format_vendor_schema(DRAFT07_SCHEMA).unwrap();
    (root, formatted)
}

/// Lay a synced-clone working tree: the catalog already holding the
/// exact desired entry for `plan`, plus the upstream vendor file equal
/// to `vendor_bytes` — the readers `run_real` consults after the sync.
#[cfg(unix)]
fn seed_clone(plan: &SchemaPlan, vendor_bytes: &str) -> tempfile::TempDir {
    let clone = tempfile::tempdir().expect("clone dir");
    let cat = catalog_with(std::slice::from_ref(&plan.desired_entry));
    let cat_abs = clone.path().join(CATALOG_PATH);
    std::fs::create_dir_all(cat_abs.parent().unwrap()).unwrap();
    std::fs::write(&cat_abs, &cat).unwrap();
    if let Some(rel) = plan.vendor_path.as_ref() {
        let abs = clone.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, vendor_bytes).unwrap();
    }
    clone
}

/// The whole-flow change-decision the cloned `run_real` loop gates each
/// schema on: a vendor schema whose catalog entry AND upstream file both
/// already match (draft-07 ⇒ no allowlist) is a CERTAIN no-op, so
/// `run_real` would `continue` past it and (if it were the only schema)
/// publish nothing. Proven against a real on-disk clone tree built by
/// the same `read_cloned_*` readers `run_real` uses — the seam between
/// the network sync and the push.
#[cfg(unix)]
#[test]
fn cloned_tree_current_vendor_is_a_certain_noop_via_run_real_readers() {
    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let (root, formatted) = seed_local_vendor_schema(&entry);
    let clone = seed_clone(&plan, &formatted);

    // Re-derive exactly as `run_real` does, off the cloned tree.
    let local = read_local_vendor_schema(root.path(), &entry).unwrap();
    let cloned_vendor = read_cloned_vendor_file(clone.path(), &plan);
    let cloned_jsonc = read_cloned_jsonc(clone.path());
    let catalog_json = std::fs::read_to_string(clone.path().join(CATALOG_PATH)).unwrap();
    let remote = RemoteState {
        catalog_json: &catalog_json,
        vendor_file: cloned_vendor.as_deref(),
        jsonc: cloned_jsonc.as_deref(),
    };
    assert!(
        !schema_change_needed(&plan, Some(&local), &remote),
        "a clone whose catalog entry + vendor file both match must be a no-op"
    );
}

/// Drift case: the cloned upstream vendor file differs from the local
/// formatted content (catalog entry unchanged). The same readers must
/// report change-needed, so `run_real` would write + splice + push —
/// the latent-bug guard, proven against a real clone tree.
#[cfg(unix)]
#[test]
fn cloned_tree_vendor_drift_needs_change_via_run_real_readers() {
    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let (root, _formatted) = seed_local_vendor_schema(&entry);
    // Seed the clone's upstream vendor file with DIFFERENT content.
    let drifted = manifest::format_vendor_schema(
        r#"{"$schema":"https://json-schema.org/draft-07/schema#","type":"string"}"#,
    )
    .unwrap();
    let clone = seed_clone(&plan, &drifted);

    let local = read_local_vendor_schema(root.path(), &entry).unwrap();
    assert_ne!(local, drifted, "fixture must differ");
    let cloned_vendor = read_cloned_vendor_file(clone.path(), &plan);
    let cloned_jsonc = read_cloned_jsonc(clone.path());
    let catalog_json = std::fs::read_to_string(clone.path().join(CATALOG_PATH)).unwrap();
    let remote = RemoteState {
        catalog_json: &catalog_json,
        vendor_file: cloned_vendor.as_deref(),
        jsonc: cloned_jsonc.as_deref(),
    };
    assert!(
        schema_change_needed(&plan, Some(&local), &remote),
        "upstream vendor drift must trigger a change even when the catalog matches"
    );
}

/// `write_vendor_schema` + `splice_entry` compose to the exact tree
/// `run_real` stages: the formatted schema lands at the vendor path AND
/// the catalog gains the desired entry. Drives the two write seams
/// `run_real`'s apply-loop calls, asserting the staged bytes.
#[cfg(unix)]
#[test]
fn write_then_splice_stages_vendor_file_and_catalog_entry() {
    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let (_root, formatted) = seed_local_vendor_schema(&entry);
    let clone = tempfile::tempdir().expect("clone");
    // Empty upstream catalog ⇒ the splice ADDS the entry.
    let cat_abs = clone.path().join(CATALOG_PATH);
    std::fs::create_dir_all(cat_abs.parent().unwrap()).unwrap();
    std::fs::write(&cat_abs, catalog_with(&[])).unwrap();

    write_vendor_schema(clone.path(), &entry, &plan, &formatted, &quiet_log())
        .expect("write vendor file into clone");
    let dest = clone.path().join("src/schemas/json/cfgd-config.json");
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        formatted,
        "staged vendor file must byte-equal the formatted schema"
    );

    let spliced = catalog::splice_entry(
        &std::fs::read_to_string(&cat_abs).unwrap(),
        &plan.desired_entry,
    )
    .expect("splice the desired entry");
    let parsed: Value = serde_json::from_str(&spliced).unwrap();
    let names: Vec<&str> = parsed["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"cfgd-config"),
        "the spliced catalog must hold the new entry; names: {names:?}"
    );
}

/// A too-high (2020-12) vendor schema whose clone's
/// `schema-validation.jsonc` lacks the `highSchemaVersion` array makes
/// `add_high_schema_version` fail — the `write_vendor_schema` allowlist
/// step must propagate that error (`schemastore: allowlist high-dialect
/// schema` context) rather than landing the schema with no allowlist
/// entry (which SchemaStore CI would then reject).
#[cfg(unix)]
#[test]
fn write_vendor_schema_propagates_allowlist_error_on_missing_array() {
    let clone = tempfile::tempdir().expect("clone");
    // jsonc present but with NO `highSchemaVersion` array ⇒ the catalog
    // helper can't find the insertion point and errors.
    let allow_abs = clone.path().join(DIALECT_ALLOWLIST_PATH);
    std::fs::create_dir_all(allow_abs.parent().unwrap()).unwrap();
    std::fs::write(&allow_abs, "{\n  \"other\": []\n}\n").unwrap();

    let entry = vendor_entry();
    let plan = plan_schema(&entry, "cfgd machine config", false, None, None).unwrap();
    let formatted = manifest::format_vendor_schema(DRAFT2020_SCHEMA).unwrap();
    let err = write_vendor_schema(clone.path(), &entry, &plan, &formatted, &quiet_log())
        .expect_err("a too-high schema with no allowlist array must error");
    assert!(
        err.to_string().contains("allowlist high-dialect schema")
            || err.to_string().contains("cfgd-config.json"),
        "expected the allowlist-failure context naming the key; got {err}"
    );
}
