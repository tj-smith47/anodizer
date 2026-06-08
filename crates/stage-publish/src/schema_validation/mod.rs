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

use std::ffi::OsString;
use std::fmt;

use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// A per-crate tag resolver: maps a crate to the release tag (monorepo prefix
/// stripped) whose version its manifest should render under. Production passes
/// [`anodizer_core::crate_scope::resolve_crate_tag`] (git-backed); tests inject
/// a fixed-tag closure so the version dimension can be exercised without a git
/// fixture.
pub type TagResolver<'a> = &'a dyn Fn(&Context, &CrateConfig) -> Option<String>;

#[cfg(test)]
mod acceptance;
mod aur;
mod chocolatey;
mod homebrew;
mod krew;
mod mcp;
mod nfpm;
mod nix;
mod scoop;
mod snapcraft;
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

/// How an external-validator run renders the offending stderr into findings.
///
/// Each language/registry publisher (aur via `bash -n`, nix via
/// `nix-instantiate --parse`, homebrew via `ruby -c`, chocolatey via
/// `xmllint --schema`) shells out to the real tool to catch a defect the
/// structural floor cannot. The scaffold around the spawn — tool-presence gate,
/// hermetic tempdir, file write, success short-circuit, and the
/// never-silent-pass empty fallback — is identical; only these fields differ.
pub(crate) struct ExternalValidator<'a> {
    /// Stable registry id stamped onto the fallback `(root)` finding, e.g.
    /// `aur` (also used in error/skip context strings).
    pub publisher: &'a str,
    /// Executable to probe and spawn, e.g. `bash`, `ruby`, `nix-instantiate`.
    pub tool: &'a str,
    /// Leading flags passed before the materialized file paths, e.g. `["-n"]`
    /// for `bash` or `["--noout", "--schema"]` for `xmllint`.
    pub flags: &'a [&'a str],
    /// Files to materialize in the tempdir as `(name, contents)`. Their written
    /// paths are appended to `flags` in this order to form the tool's argv, so
    /// validators needing a vendored schema (chocolatey: `<xsd> <nuspec>`) list
    /// the schema first. Publishers that validate a single rendered artifact
    /// pass one entry.
    pub files: &'a [(&'a str, &'a str)],
    /// `verbose` line logged when `tool` is absent. Returning no findings on a
    /// missing tool is intentional — the structural floor stands and a missing
    /// tool is never an artifact defect.
    pub skip_message: &'a str,
    /// `(root)` finding expectation when the tool fails but emits no parseable
    /// diagnostic. Returning empty here would silently report a failed
    /// validator as PASS, so a real failure must always surface.
    pub empty_fallback: &'a str,
}

/// Run an external syntax/schema validator over freshly rendered publisher
/// artifacts, returning one [`SchemaFinding`] per diagnostic.
///
/// Probes `cfg.tool`; if absent, logs `cfg.skip_message` and returns no findings
/// (a missing tool is never a defect). Otherwise writes every `cfg.files` entry
/// into a hermetic tempdir, spawns `cfg.tool` with `cfg.flags` followed by the
/// written paths in order, and — on a non-zero exit — runs `parse_stderr` over
/// the tool's stderr. A non-zero exit that yields no parseable finding still
/// emits a `(root)` finding carrying `cfg.empty_fallback` (or the raw stderr
/// when present), so a failed validator never reads as PASS. A clean exit
/// returns no findings.
pub(crate) fn run_external_validator<P>(
    cfg: &ExternalValidator<'_>,
    parse_stderr: P,
    log: &StageLogger,
) -> Result<Vec<SchemaFinding>>
where
    P: FnOnce(&str) -> Vec<SchemaFinding>,
{
    let publisher = cfg.publisher;
    let tool = cfg.tool;

    if !anodizer_core::tool_detect::tool_available(tool).unwrap_or(false) {
        log.verbose(cfg.skip_message);
        return Ok(Vec::new());
    }

    let dir = tempfile::tempdir()
        .with_context(|| format!("{publisher}: create temp dir for {tool} validation"))?;
    let mut args: Vec<OsString> = cfg.flags.iter().map(OsString::from).collect();
    for (name, contents) in cfg.files {
        let path = dir.path().join(name);
        std::fs::write(&path, contents)
            .with_context(|| format!("{publisher}: write {name} for {tool}"))?;
        args.push(path.into_os_string());
    }

    let output = std::process::Command::new(tool)
        .args(&args)
        .output()
        .with_context(|| format!("{publisher}: run {tool}"))?;
    if output.status.success() {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut findings = parse_stderr(&stderr);
    if findings.is_empty() {
        let trimmed = stderr.trim();
        let expected = if trimmed.is_empty() {
            cfg.empty_fallback.to_string()
        } else {
            trimmed.to_string()
        };
        findings.push(SchemaFinding {
            publisher: publisher.to_string(),
            field: "(root)".to_string(),
            expected,
        });
    }
    Ok(findings)
}

/// Run a single crate's manifest render+validate body with that crate's OWN
/// version/name/tag template vars in scope, restoring the prior scope after.
///
/// Each per-crate validator wraps its render call in this helper so the
/// validated manifest carries the crate's own version — matching what the live
/// publish path renders. In workspace per-crate independent-version mode the
/// global `Version` is the FIRST crate's, so an unscoped render would validate
/// a per-field `PackageVersion` (etc.) against the wrong version. In
/// single-crate / lockstep mode the per-crate tag resolves to the same version
/// the global context already carries, so behavior is identical.
///
/// `crate_cfg` is looked up from the crate universe by name; `resolve_tag` is
/// the per-crate tag source (production [`resolve_crate_tag`]; tests a fixed
/// closure). Fails loud when the crate is absent or has no resolvable tag —
/// the same fail-loud contract the live path carries.
pub(crate) fn with_validated_crate_scope<T>(
    ctx: &mut Context,
    crate_name: &str,
    resolve_tag: TagResolver<'_>,
    body: impl FnOnce(&mut Context) -> Result<T>,
) -> Result<T> {
    let crate_cfg = crate::util::all_crates(ctx)
        .into_iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "schema-validation: crate '{crate_name}' is not present in the crate universe"
            )
        })?;
    anodizer_core::crate_scope::with_crate_scope(ctx, &crate_cfg, resolve_tag, body)
}

/// A publisher's self-contained artifact-schema validator.
///
/// Each implementation renders the manifest(s) the publisher would emit for
/// every in-scope crate — in-memory, with no side effects — and validates each
/// against its vendored registry schema, returning a [`SchemaFinding`] per
/// violation. `Ok(vec![])` means every rendered artifact conforms.
#[allow(clippy::type_complexity)]
pub(crate) trait PublisherSchemaValidator: Send + Sync {
    /// Stable registry id of this publisher, e.g. `winget`.
    fn publisher(&self) -> &'static str;

    /// Render this publisher's configured artifact(s) for every in-scope crate
    /// and validate them against the vendored schema. `Ok(vec![])` means pass.
    ///
    /// `resolve_tag` supplies each crate's release tag so a per-crate render
    /// can be scoped to the crate's own version (via
    /// [`with_validated_crate_scope`]). The validator takes `&mut Context`
    /// because that scoping mutates and restores the template vars.
    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>>;
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
        Box::new(mcp::McpSchemaValidator),
        Box::new(chocolatey::ChocolateySchemaValidator),
        Box::new(snapcraft::SnapcraftSchemaValidator),
        Box::new(homebrew::HomebrewSchemaValidator),
        Box::new(nfpm::NfpmSchemaValidator),
        Box::new(aur::AurSchemaValidator),
        Box::new(nix::NixSchemaValidator),
    ]
}

/// Render and schema-validate every registered publisher's artifacts for the
/// in-scope crates, failing loud on the first run that produces any violations.
///
/// Drives every validator from [`validators`], aggregates all findings, and —
/// if any exist — aborts with a multi-line message listing each violation by
/// publisher, field, and expectation. With no registered validators this is a
/// no-op that returns `Ok(())`.
///
/// `resolve_tag` is threaded to each validator so its per-crate render is
/// scoped to that crate's own version. Production callers pass
/// [`anodizer_core::crate_scope::resolve_crate_tag`]; tests inject a fixed-tag
/// closure.
pub fn validate_publisher_schemas(
    ctx: &mut Context,
    log: &StageLogger,
    resolve_tag: TagResolver<'_>,
) -> Result<()> {
    let mut findings: Vec<SchemaFinding> = Vec::new();
    for validator in validators() {
        let publisher = validator.publisher();
        let result = validator
            .validate(ctx, resolve_tag)
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

/// Test-only per-crate tag resolver: returns the version currently scoped on
/// `ctx` as the crate's tag, so [`with_validated_crate_scope`] re-derives the
/// SAME version the test pre-set. Lets validator unit tests exercise the
/// per-crate-scoped render path without a git fixture: a test that scopes
/// `Version` (single-crate / lockstep / per-crate-via-`--crate`) keeps that
/// version through the scope.
#[cfg(test)]
pub(crate) fn test_current_version_resolver() -> impl Fn(&Context, &CrateConfig) -> Option<String> {
    |ctx: &Context, _: &CrateConfig| {
        let v = ctx.version();
        if v.trim().is_empty() { None } else { Some(v) }
    }
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
