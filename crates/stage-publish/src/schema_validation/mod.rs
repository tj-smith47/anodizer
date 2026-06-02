//! Offline schema validation of generated publisher artifacts.
//!
//! Each package-manager publisher renders a manifest — a winget YAML, a scoop
//! JSON, a krew plugin spec, a chocolatey nuspec, and so on — whose shape the
//! destination registry enforces at submission time. Required-field presence
//! checks elsewhere prove the inputs are populated, but they do not prove the
//! *whole rendered document* conforms to the registry's published schema: a
//! wrong-typed value, a misnamed key, or an out-of-enum field sails through
//! every local check and is only rejected after a real release has already
//! uploaded the manifest.
//!
//! This module is the shared foundation for closing that gap. It vendors the
//! registry schemas (offline, pinned — see `schemas/SOURCES.md`), exposes a
//! [`validate_json`] helper that runs any instance against any JSON Schema and
//! reports each violation as a field-named [`SchemaFinding`], and defines the
//! [`PublisherSchemaValidator`] trait each publisher implements to render and
//! check its own artifacts. [`validate_publisher_schemas`] drives every
//! registered validator and fails the snapshot/dry-run pass loud, naming the
//! publisher, the offending field, and what the schema expected.

use std::fmt;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use serde_json::Value;

mod krew;
mod scoop;
mod winget;

/// A single schema-conformance violation in a rendered publisher artifact.
///
/// Carries enough to point an operator straight at the defect: which publisher
/// produced the artifact, the JSON-Pointer path to the offending field, and the
/// registry schema's own expectation for that field.
#[derive(Debug)]
pub(crate) struct SchemaFinding {
    /// Stable registry id of the publisher whose artifact failed, e.g. `winget`.
    pub publisher: String,
    /// JSON-Pointer path to the offending field (e.g. `/PackageVersion`), or
    /// `(root)` when the violation is on the document itself (e.g. a missing
    /// top-level required key).
    pub field: String,
    /// The registry schema's expectation for the field — the validator's own
    /// error message, e.g. `"oops" is not of type "number"`.
    pub expected: String,
}

impl fmt::Display for SchemaFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: field '{}' — {}",
            self.publisher, self.field, self.expected
        )
    }
}

/// Validate a JSON `instance` against the JSON-Schema text `schema_src`,
/// returning one [`SchemaFinding`] per violation (an empty Vec means the
/// instance conforms).
///
/// `publisher` is the registry id stamped onto each finding. `schema_src` is the
/// vendored schema JSON text (embedded via `include_str!`). A malformed schema
/// or schema text is an error, not a finding — the vendored schema is the tool's
/// own asset, so a parse failure is a bug to surface, never a manifest defect.
pub(crate) fn validate_json(
    publisher: &str,
    instance: &Value,
    schema_src: &str,
) -> Result<Vec<SchemaFinding>> {
    let schema: Value = serde_json::from_str(schema_src)
        .with_context(|| format!("parse vendored schema for publisher '{publisher}'"))?;
    let validator = jsonschema::validator_for(&schema)
        .with_context(|| format!("compile vendored schema for publisher '{publisher}'"))?;

    let findings = validator
        .iter_errors(instance)
        .map(|error| {
            let path = error.instance_path().as_str();
            let field = if path.is_empty() {
                "(root)".to_string()
            } else {
                path.to_string()
            };
            SchemaFinding {
                publisher: publisher.to_string(),
                field,
                expected: error.to_string(),
            }
        })
        .collect();
    Ok(findings)
}

/// Convert a YAML manifest into a [`serde_json::Value`] so YAML publishers
/// (winget, snapcraft, krew, …) can reuse [`validate_json`] against a JSON
/// Schema. The JSON data model is a superset of what these manifests use, so
/// the round-trip is lossless for validation purposes.
pub(crate) fn yaml_to_json(yaml: &str) -> Result<Value> {
    serde_yaml_ng::from_str(yaml).context("parse publisher manifest as YAML")
}

/// A publisher's self-contained artifact-schema validator.
///
/// Each implementation renders the manifest(s) the publisher would emit for
/// every in-scope crate — in-memory, with no side effects — and validates each
/// against its vendored registry schema, returning a [`SchemaFinding`] per
/// violation. `Ok(vec![])` means every rendered artifact conforms.
pub(crate) trait PublisherSchemaValidator: Send + Sync {
    /// Stable registry id of this publisher, e.g. `winget`.
    fn publisher(&self) -> &'static str;

    /// Render this publisher's configured artifact(s) for every in-scope crate
    /// and validate them against the vendored schema. `Ok(vec![])` means pass.
    fn validate(&self, ctx: &Context) -> Result<Vec<SchemaFinding>>;
}

/// The registered set of per-publisher schema validators.
///
/// Each per-publisher implementation appends its validator here so
/// [`validate_publisher_schemas`] picks it up automatically.
fn validators() -> Vec<Box<dyn PublisherSchemaValidator>> {
    vec![
        Box::new(winget::WingetSchemaValidator),
        Box::new(scoop::ScoopSchemaValidator),
        Box::new(krew::KrewSchemaValidator),
    ]
}

/// Render and schema-validate every registered publisher's artifacts for the
/// in-scope crates, failing loud on the first run that produces any violations.
///
/// Drives every validator from [`validators`], aggregates all findings, and —
/// if any exist — aborts with a multi-line message listing each violation by
/// publisher, field, and expectation. With no registered validators this is a
/// no-op that returns `Ok(())`.
pub(crate) fn validate_publisher_schemas(ctx: &Context, log: &StageLogger) -> Result<()> {
    let mut findings: Vec<SchemaFinding> = Vec::new();
    for validator in validators() {
        let publisher = validator.publisher();
        let result = validator
            .validate(ctx)
            .with_context(|| format!("schema-validate publisher '{publisher}' artifacts"))?;
        log.verbose(&format!(
            "schema-validation: publisher '{}' produced {} finding(s)",
            publisher,
            result.len()
        ));
        findings.extend(result);
    }

    if findings.is_empty() {
        return Ok(());
    }

    let mut message = String::from("publisher artifact schema validation failed:");
    for finding in &findings {
        message.push('\n');
        message.push_str(&finding.to_string());
    }
    bail!(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA: &str = r#"{
        "type": "object",
        "required": ["name"],
        "properties": {
            "age": { "type": "number" }
        }
    }"#;

    #[test]
    fn wrong_typed_field_is_reported_with_its_pointer_path() {
        let instance = json!({ "name": "ok", "age": "oops" });
        let findings = validate_json("winget", &instance, SCHEMA).expect("validation runs");

        let age = findings
            .iter()
            .find(|f| f.field == "/age")
            .expect("a finding for the wrong-typed /age field");
        assert_eq!(age.publisher, "winget");
        assert!(
            age.expected.contains("number"),
            "expected message names the schema type, got: {}",
            age.expected
        );
    }

    #[test]
    fn missing_required_field_is_reported_at_root() {
        let instance = json!({ "age": "oops" });
        let findings = validate_json("winget", &instance, SCHEMA).expect("validation runs");

        let required = findings
            .iter()
            .find(|f| f.field == "(root)")
            .expect("a root finding for the missing required key");
        assert!(
            required.expected.contains("name"),
            "expected message names the missing key, got: {}",
            required.expected
        );

        // The wrong-typed field is independently reported alongside it.
        assert!(
            findings.iter().any(|f| f.field == "/age"),
            "both the missing-required and wrong-typed violations are surfaced"
        );
    }

    #[test]
    fn conforming_instance_yields_no_findings() {
        let instance = json!({ "name": "ok", "age": 42 });
        let findings = validate_json("winget", &instance, SCHEMA).expect("validation runs");
        assert!(
            findings.is_empty(),
            "a conforming instance must produce zero findings, got: {findings:?}"
        );
    }

    #[test]
    fn finding_display_renders_one_line() {
        let finding = SchemaFinding {
            publisher: "winget".to_string(),
            field: "/PackageVersion".to_string(),
            expected: r#""1" is not of type "number""#.to_string(),
        };
        assert_eq!(
            finding.to_string(),
            r#"winget: field '/PackageVersion' — "1" is not of type "number""#
        );
    }

    #[test]
    fn yaml_manifest_round_trips_to_json_for_validation() {
        let yaml = "name: ok\nage: oops\n";
        let instance = yaml_to_json(yaml).expect("yaml parses");
        let findings = validate_json("winget", &instance, SCHEMA).expect("validation runs");
        assert!(
            findings.iter().any(|f| f.field == "/age"),
            "a YAML manifest reuses the same JSON-Schema check"
        );
    }
}
