//! nfpm config + built-package schema validation.
//!
//! anodizer does not implement Linux packaging itself: it renders an nfpm YAML
//! config per crate per (target × format) and shells out to the `nfpm` CLI,
//! which builds the `.deb` / `.rpm` / `.apk` and stamps the package control
//! metadata from that config. Two layers guard against shipping a
//! registry-rejected or mislabeled package:
//!
//! 1. **Primary (hermetic, always on):** the generated nfpm YAML is validated
//!    against nfpm's own draft-2020-12 config schema. The schema is
//!    `additionalProperties: false` and requires `name` / `arch` / `version`,
//!    so a misnamed key, a wrong-typed value, or a missing required field
//!    surfaces in the snapshot/dry-run pass — not after a release uploads a
//!    broken package.
//! 2. **Secondary (gated):** when the nfpm stage already built a package in
//!    this run and the inspection tool is present (`dpkg-deb` for `.deb`,
//!    `rpm` for `.rpm`), the actual control fields are read back and compared
//!    to the resolved config, catching a drift between what anodizer
//!    generated and what the package physically carries.
//!
//! The expected control values are read from the generated YAML this validator
//! already schema-checked, so both layers share one source of truth.

use std::path::Path;
use std::process::Command;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde_json::Value;

use super::{
    PublisherSchemaValidator, SchemaFinding, TagResolver, validate_json,
    with_validated_crate_scope, yaml_to_json,
};

/// nfpm's own config schema (draft 2020-12), pinned to the nfpm version
/// `crates/stage-nfpm` targets. Embedded so the primary layer is fully
/// offline; refresh via `schemas/SOURCES.md`.
const NFPM_SCHEMA: &str = include_str!("../../schemas/nfpm.schema.json");

/// Validates anodizer's rendered nfpm configs against nfpm's config schema and,
/// when the inspection tools are present, cross-checks the control fields of
/// any `.deb` / `.rpm` already built this run.
pub(crate) struct NfpmSchemaValidator;

/// True iff the crate carries at least one nfpm config — the same universe the
/// build's `run` loop iterates (`c.nfpms.is_some()`, where an empty list yields
/// no packages).
fn is_nfpm_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .find(|c| c.name == crate_name)
        .and_then(|c| c.nfpms)
        .is_some_and(|cfgs| !cfgs.is_empty())
}

impl PublisherSchemaValidator for NfpmSchemaValidator {
    fn publisher(&self) -> &'static str {
        "nfpm"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk the nfpm-configured crates (honoring `--crate` selection, else
        // every nfpm-configured crate) so the validated set equals the built
        // set. Both the build's `run` and the offline renderer resolve a crate
        // via `ctx.config.crates`, so a crate configured only under
        // `workspaces[].crates` is built by neither and validated by neither.
        // The version that NAMED the produced packages: the global `Version`
        // var the build's nfpm stage stamped into every crate's `version:`
        // field (in snapshot, the `<base>-SNAPSHOT-<sha>` synthesized version).
        // Captured before the per-crate scope below resets it to a bare,
        // snapshot-label-stripped tag re-derivation.
        let artifact_version = ctx.version();

        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_nfpm_per_crate_configured);
        for crate_name in &selected {
            if !is_nfpm_per_crate_configured(ctx, crate_name) {
                continue;
            }

            // Validate under THIS crate's own name/tag scope so per-crate
            // templated name/content fields resolve per crate, but render the
            // `version:` from `artifact_version` (see
            // `render_build_matched_nfpm_configs`) so it equals what the build
            // stamped in every config mode.
            let crate_findings = with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                let mut out = Vec::new();
                // One rendered config per (config × target × format). An empty Vec
                // means there is nothing to validate — the configs were all
                // `if:`-suppressed / format-less, the `ids` filter admitted none,
                // or no packaging-eligible artifact was built for the crate in
                // this snapshot shard (the same shard-tolerance cases the build
                // skips).
                let configs =
                    render_build_matched_nfpm_configs(ctx, crate_name, &artifact_version)?;
                if configs.is_empty() {
                    log.verbose(&format!(
                        "crate '{}' produced no nfpm config in this snapshot \
                         shard (skipped or no eligible artifact); skipping schema validation",
                        crate_name
                    ));
                    return Ok(out);
                }
                for cfg in &configs {
                    let value = yaml_to_json(&cfg.yaml)?;
                    out.extend(validate_json("nfpm", &value, NFPM_SCHEMA)?);
                }
                out.extend(validate_built_packages(ctx, crate_name, &configs, &log)?);
                Ok(out)
            })?;
            findings.extend(crate_findings);
        }

        Ok(findings)
    }
}

/// Render `crate_name`'s nfpm configs exactly as the build stage stamped them.
///
/// The build's nfpm stage reads the global `Version` template var ONCE and
/// feeds it to every crate's package `version:` — in snapshot/dry-run that is
/// the artifact-naming `<base>-SNAPSHOT-<sha>` version. The surrounding
/// [`with_validated_crate_scope`] re-derives a per-crate *bare* tag version
/// (`resolve_crate_tag` strips the snapshot label and any monorepo prefix), so
/// rendering straight through it would stamp `0.4.0` while the build stamped
/// `0.4.0-SNAPSHOT-<sha>`, and the gated control cross-check would reject every
/// snapshot package. Pin `Version`/`RawVersion` back to `artifact_version`
/// (keeping the per-crate `Name`/`Tag`/`ProjectName` scope for templated
/// name/content fields) so the rendered `version:` equals what was built —
/// single-crate, workspace-lockstep, and per-crate-independent modes alike,
/// since each mode's build read the same global `Version` this captures. A
/// blank `artifact_version` leaves the scope's value untouched. Mirrors the
/// binstall/nix `scope_artifact_version` cross-check, which solved the same
/// "render the version that named the artifacts, not a tag re-derivation"
/// problem.
fn render_build_matched_nfpm_configs(
    ctx: &mut Context,
    crate_name: &str,
    artifact_version: &str,
) -> Result<Vec<anodizer_stage_nfpm::NfpmRenderedConfig>> {
    if !artifact_version.trim().is_empty() {
        ctx.template_vars_mut().set("Version", artifact_version);
        ctx.template_vars_mut().set("RawVersion", artifact_version);
    }
    anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(ctx, crate_name)
}

/// The control fields the gated layer asserts a built package carries, derived
/// from the generated YAML the primary layer already validated.
struct ExpectedControl {
    /// nfpm package name: the YAML `name`, falling back to nfpm's own default
    /// (the package name resolves elsewhere) — `None` when neither the config
    /// nor a default is known, in which case the name check is skipped.
    name: Option<String>,
    version: String,
    /// The architecture the built package's `Architecture` control field must
    /// equal, already translated from the generic nfpm arch into the
    /// packager's control nomenclature (deb keeps `arm64`, rpm uses
    /// `aarch64`). This is the load-bearing regression guard for the arch bug:
    /// a package whose control arch drifts from the resolved arch bites here.
    arch: String,
    /// `Some` when the config sets `maintainer`; the control-field check then
    /// asserts the built package carries one.
    maintainer: Option<String>,
}

/// Cross-check every already-built `.deb` / `.rpm` for the crate against the
/// resolved config, when the matching inspection tool is on `PATH`.
///
/// Each `LinuxPackage` artifact is matched to the rendered config for its
/// format, the package's control fields are read back, and a mismatch (or a
/// non-zero tool exit with no parsed field) becomes a [`SchemaFinding`]. When
/// the tool is absent the layer logs a `verbose` skip and relies on the
/// primary schema floor — a missing tool is never a package defect.
fn validate_built_packages(
    ctx: &Context,
    crate_name: &str,
    configs: &[anodizer_stage_nfpm::NfpmRenderedConfig],
    log: &StageLogger,
) -> Result<Vec<SchemaFinding>> {
    let mut findings = Vec::new();

    for artifact in ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::LinuxPackage, crate_name)
    {
        let path = artifact.path.as_path();
        // The secondary layer inspects a package physically on disk. A
        // snapshot/dry-run registers the predicted `LinuxPackage` path without
        // building the file, so a missing file means the build did not run
        // here — the primary schema floor already covered the rendered config.
        if !path.exists() {
            log.verbose(&format!(
                "nfpm package {} not built in this run; relying on the config \
                 schema floor",
                path.display()
            ));
            continue;
        }
        let format = artifact.metadata.get("format").map(String::as_str);
        let target = artifact.target.as_deref().unwrap_or("");

        // Pair the built package with the config it was rendered from so the
        // expected control values come from the same source the schema layer
        // validated. Match on (format, target) — the keys that uniquely
        // identify one rendered config.
        let Some(cfg) = configs
            .iter()
            .find(|c| Some(c.format.as_str()) == format && c.target == target)
        else {
            continue;
        };
        let expected = expected_control(&cfg.yaml, &cfg.format)?;

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "deb" => {
                if anodizer_core::tool_detect::tool_available("dpkg-deb").unwrap_or(false) {
                    findings.extend(check_deb_control(path, &expected)?);
                } else {
                    log.verbose(
                        "dpkg-deb not on PATH — relying on the nfpm config schema floor \
                         for .deb validation",
                    );
                }
            }
            "rpm" => {
                if anodizer_core::tool_detect::tool_available("rpm").unwrap_or(false) {
                    findings.extend(check_rpm_control(path, &expected)?);
                } else {
                    log.verbose(
                        "rpm not on PATH — relying on the nfpm config schema floor \
                         for .rpm validation",
                    );
                }
            }
            // Other packagers (apk, archlinux, ipk) have no portable
            // always-present inspector here; the primary schema floor stands.
            _ => {}
        }
    }

    Ok(findings)
}

/// Read the load-bearing control values (`name`, `version`, `arch`,
/// `maintainer`) from a generated nfpm YAML for cross-checking against the
/// built package. `format` selects the packager-specific arch nomenclature the
/// built package stamps, so the expected `Architecture` matches what the tool
/// reports.
fn expected_control(yaml: &str, format: &str) -> Result<ExpectedControl> {
    let value = yaml_to_json(yaml)?;
    let str_at = |ptr: &str| {
        value
            .pointer(ptr)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let arch = str_at("/arch")
        .map(|a| anodizer_stage_nfpm::control_arch(format, &a))
        .unwrap_or_default();
    Ok(ExpectedControl {
        name: str_at("/name"),
        version: expected_version(
            &str_at("/version").unwrap_or_default(),
            str_at("/prerelease").as_deref(),
            str_at("/version_metadata").as_deref(),
        ),
        arch,
        maintainer: str_at("/maintainer"),
    })
}

/// Compose the upstream-grammar version the built package is expected to carry
/// from the nfpm config's separate `version` / `prerelease` / `version_metadata`
/// fields, mirroring nfpm's own composition (`<version>~<prerelease>+<meta>`).
/// The deb `epoch:` prefix and the trailing numeric `-release` revision are
/// deliberately NOT appended — [`version_matches`] strips/tolerates those on the
/// built side. Without folding in `prerelease`, a snapshot package (whose
/// `~SNAPSHOT-<sha>` lives entirely in `prerelease`) would compare its full
/// `0.4.0~SNAPSHOT-<sha>` against a bare `0.4.0` and spuriously fail.
fn expected_version(
    version: &str,
    prerelease: Option<&str>,
    version_metadata: Option<&str>,
) -> String {
    let mut out = version.to_string();
    if let Some(pre) = prerelease.filter(|s| !s.is_empty()) {
        out.push('~');
        out.push_str(pre);
    }
    if let Some(meta) = version_metadata.filter(|s| !s.is_empty()) {
        out.push('+');
        out.push_str(meta);
    }
    out
}

/// Inspect a built `.deb`'s control fields via `dpkg-deb -f` and assert they
/// match the resolved config. A mismatch — or a non-zero `dpkg-deb` exit with
/// no parseable control field — becomes a finding; an empty Vec means the
/// package's control matches.
fn check_deb_control(path: &Path, expected: &ExpectedControl) -> Result<Vec<SchemaFinding>> {
    let output = Command::new("dpkg-deb")
        .arg("-f")
        .arg(path)
        .arg("Package")
        .arg("Version")
        .arg("Architecture")
        .arg("Maintainer")
        .output()
        .context("nfpm: run dpkg-deb -f")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(vec![tool_failure_finding("dpkg-deb", path, &stderr)]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fields = parse_control_fields(&stdout);
    Ok(compare_control("deb", path, expected, &fields))
}

/// Inspect a built `.rpm`'s header via `rpm -qip` and assert the headline
/// fields match the resolved config. A mismatch — or a non-zero `rpm` exit
/// with no parseable field — becomes a finding.
fn check_rpm_control(path: &Path, expected: &ExpectedControl) -> Result<Vec<SchemaFinding>> {
    let output = Command::new("rpm")
        .arg("-qip")
        .arg(path)
        .output()
        .context("nfpm: run rpm -qip")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(vec![tool_failure_finding("rpm", path, &stderr)]);
    }

    // `rpm -qip` prints `Field       : value` lines; the field labels differ
    // from deb's, so normalize to the same {Package, Version, Architecture,
    // Maintainer} keys `compare_control` expects. RPM has no maintainer
    // concept, so the maintainer assertion is deb-only.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = parse_rpm_fields(&stdout);
    // RPM carries no Maintainer header; suppress that comparison by mirroring
    // the expected value so it never reports a spurious mismatch.
    if let Some(m) = expected.maintainer.as_deref() {
        fields.insert("Maintainer".to_string(), m.to_string());
    }
    Ok(compare_control("rpm", path, expected, &fields))
}

/// Compare parsed control `fields` against the `expected` config values,
/// returning one finding per load-bearing mismatch.
fn compare_control(
    format: &str,
    path: &Path,
    expected: &ExpectedControl,
    fields: &std::collections::BTreeMap<String, String>,
) -> Vec<SchemaFinding> {
    let display = path.display();
    let finding = |field: &str, msg: String| SchemaFinding {
        publisher: "nfpm".to_string(),
        field: format!("{format}:{field}"),
        expected: msg,
    };
    let mut findings = Vec::new();

    if let Some(expected_name) = expected.name.as_deref() {
        match fields.get("Package").map(String::as_str) {
            Some(actual) if actual == expected_name => {}
            other => findings.push(finding(
                "Package",
                format!(
                    "built {display} carries package name {other:?}, config resolved \
                     {expected_name:?}"
                ),
            )),
        }
    }

    match fields.get("Version").map(String::as_str) {
        Some(actual) if version_matches(actual, &expected.version) => {}
        other => findings.push(finding(
            "Version",
            format!(
                "built {display} carries version {other:?}, config resolved {:?}",
                expected.version
            ),
        )),
    }

    // The built package's Architecture must EQUAL the resolved arch (in the
    // packager's nomenclature) — presence alone would let the arch-mislabel
    // bug this task fixed regress undetected.
    match fields.get("Architecture").map(String::as_str) {
        Some(actual) if actual == expected.arch => {}
        other => findings.push(finding(
            "Architecture",
            format!(
                "built {display} carries architecture {other:?}, config resolved {:?}",
                expected.arch
            ),
        )),
    }

    if expected.maintainer.is_some() && fields.get("Maintainer").is_none_or(|m| m.is_empty()) {
        findings.push(finding(
            "Maintainer",
            format!(
                "config sets a maintainer but built {display} carries no Maintainer \
                 control field"
            ),
        ));
    }

    findings
}

/// A package built with an epoch/release renders its control `Version` with an
/// epoch prefix (`1:`) or release suffix (`-1`); the config's `version` is the
/// bare upstream version. Treat the control field as matching when it reduces to
/// the expected upstream version under nfpm's packager-grammar rewrites.
fn version_matches(actual: &str, expected: &str) -> bool {
    // nfpm rewrites the upstream version into each packager's grammar before it
    // lands in the control metadata: deb and rpm reserve `-` as the
    // version/release delimiter, so they substitute a `-` in the version core
    // with `~` (pre-release sort order) or `_`; deb additionally prepends an
    // `epoch:` and appends a numeric `-release` revision. Reduce both operands
    // toward the bare upstream version before comparing, so an
    // epoch/release/pre-release package is not flagged as a spurious mismatch.
    let canon = |s: &str| s.replace(['~', '_'], "-");
    let expected = canon(expected);
    let actual_full = canon(actual);
    // Drop a leading deb `epoch:`.
    let actual = actual_full
        .split_once(':')
        .map_or(actual_full.as_str(), |(_, rest)| rest);
    if actual == expected {
        return true;
    }
    // Accept a trailing nfpm `-release` revision (deb embeds it in Version, e.g.
    // `<upstream>-1`); the revision must be numeric so a genuinely different
    // upstream (`1.0.0-rc1` vs `1.0.0`) still fails.
    actual
        .strip_prefix(expected.as_str())
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rev| !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_digit()))
}

/// Build the finding emitted when an inspection tool exits non-zero — a real
/// failure must always surface rather than silently passing.
fn tool_failure_finding(tool: &str, path: &Path, stderr: &str) -> SchemaFinding {
    let trimmed = stderr.trim();
    let detail = if trimmed.is_empty() {
        format!(
            "{tool} exited non-zero inspecting {} with no diagnostic",
            path.display()
        )
    } else {
        format!("{tool} failed inspecting {}: {trimmed}", path.display())
    };
    SchemaFinding {
        publisher: "nfpm".to_string(),
        field: "(package)".to_string(),
        expected: detail,
    }
}

/// Parse `Field: value` control lines (the `dpkg-deb -f` output shape) into a
/// field map.
fn parse_control_fields(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Parse `rpm -qip` header output into the same {Package, Version,
/// Architecture} keys `compare_control` expects. `rpm` labels them `Name`,
/// `Version`, and `Architecture`, so `Name` is remapped to `Package`.
fn parse_rpm_fields(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().to_string();
        match key {
            "Name" => {
                out.insert("Package".to_string(), value);
            }
            "Version" => {
                out.insert("Version".to_string(), value);
            }
            "Architecture" => {
                out.insert("Architecture".to_string(), value);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        CrateConfig, NfpmConfig, NfpmContent, NfpmDebConfig, NfpmRpmConfig, PublishConfig,
        ReleaseConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;
    use serde_json::Value;

    use super::*;

    /// An `NfpmConfig` exercising every YAML-affecting field with values nfpm's
    /// schema accepts.
    fn every_option_nfpm_cfg() -> NfpmConfig {
        NfpmConfig {
            package_name: Some("widget".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            vendor: Some("Acme Corp".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            maintainer: Some("Acme <ops@acme.example>".to_string()),
            description: Some("A widget management tool".to_string()),
            license: Some("MIT".to_string()),
            section: Some("utils".to_string()),
            priority: Some("optional".to_string()),
            recommends: Some(vec!["widget-extras".to_string()]),
            suggests: Some(vec!["widget-docs".to_string()]),
            conflicts: Some(vec!["old-widget".to_string()]),
            replaces: Some(vec!["legacy-widget".to_string()]),
            provides: Some(vec!["widget-cli".to_string()]),
            contents: Some(vec![NfpmContent {
                src: "/etc/widget/config.toml".to_string(),
                dst: "/etc/widget/config.toml".to_string(),
                content_type: Some("config".to_string()),
                file_info: None,
                packager: None,
                expand: None,
            }]),
            deb: Some(NfpmDebConfig {
                compression: Some("xz".to_string()),
                ..Default::default()
            }),
            rpm: Some(NfpmRpmConfig {
                summary: Some("Widget management".to_string()),
                compression: Some("xz".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn nfpm_crate(crate_name: &str, tag_template: &str, cfg: NfpmConfig) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig::default()),
            nfpms: Some(vec![cfg]),
            ..Default::default()
        }
    }

    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    fn add_linux_binary(ctx: &mut Context, crate_name: &str, binary: &str) {
        add_linux_binary_on_target(ctx, crate_name, binary, "x86_64-unknown-linux-gnu");
    }

    fn add_linux_binary_on_target(ctx: &mut Context, crate_name: &str, binary: &str, target: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from(format!("/dist/{binary}")),
            name: binary.to_string(),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    /// Add an amd64 Linux binary carrying `amd64_variant` (GOAMD64 microarch)
    /// build metadata — the input the build auto-derives `deb.arch_variant`
    /// from.
    fn add_linux_binary_with_variant(
        ctx: &mut Context,
        crate_name: &str,
        binary: &str,
        variant: &str,
    ) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from(format!("/dist/{binary}")),
            name: binary.to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
            size: None,
        });
    }

    /// `version_matches` must equate the bare upstream config version with the
    /// control `Version` nfpm renders into each packager's grammar: a leading
    /// deb `epoch:`, a trailing numeric `-release` revision, and the `~`/`_`
    /// substitutions deb/rpm apply to a `-` in the version core (which they
    /// reserve as the version/release delimiter). A genuinely different upstream
    /// version must still fail.
    #[test]
    fn version_matches_normalizes_nfpm_grammar() {
        // Exact, epoch-only, and release-only forms.
        assert!(version_matches("1.0.0", "1.0.0"));
        assert!(version_matches("1:1.0.0", "1.0.0"));
        assert!(version_matches("1.0.0-1", "1.0.0"));
        assert!(version_matches("1:1.0.0-1", "1.0.0"));
        // deb pre-release: epoch + `~`-substituted first `-` + multi-`-` core +
        // numeric release (the cfgd snapshot package that exposed the bug).
        assert!(version_matches(
            "1:0.4.0~SNAPSHOT-3d07f6c-1",
            "0.4.0-SNAPSHOT-3d07f6c"
        ));
        // rpm pre-release: `~` then `_` substitution, release carried separately.
        assert!(version_matches(
            "0.4.0~SNAPSHOT_3d07f6c",
            "0.4.0-SNAPSHOT-3d07f6c"
        ));
        // Bare-version mismatch: a different patch level must not match.
        assert!(!version_matches("0.4.0", "0.5.0"));
        // A different upstream version must NOT be masked by the normalization.
        assert!(!version_matches("1:2.0.0-1", "1.0.0"));
        // A non-numeric trailing token is a different version, not a revision.
        assert!(!version_matches("1.0.0-rc1", "1.0.0"));
        // Distinct pre-release labels must not collapse together: `~SNAPSHOT`
        // (deb-rendered) is not `~RELEASE`.
        assert!(!version_matches("0.4.0~SNAPSHOT", "0.4.0-RELEASE"));
        // The af44f7ab truncation guard: the naive `split('-').next()` the bug
        // used would reduce both stems to `0.4.0` and FALSELY match; a different
        // multi-`-` upstream must still fail under the real normalization.
        assert!(!version_matches(
            "1:0.4.0~SNAPSHOT-aaaaaaa-1",
            "0.4.0-SNAPSHOT-bbbbbbb"
        ));
        // A longer core is not a revision match.
        assert!(!version_matches("1.0.0.5", "1.0.0"));
    }

    /// `expected_control` must fold the nfpm config's separate `prerelease`
    /// (and `version_metadata`) fields into the expected version — not read the
    /// bare `/version` alone. Regression for the snapshot bug: under `--snapshot`
    /// the version core stays `0.4.0` and the `~SNAPSHOT-<sha>` lives entirely in
    /// `prerelease`, so a `/version`-only expected (`0.4.0`) spuriously failed
    /// against the built `1:0.4.0~SNAPSHOT-<sha>-1` deb / `0.4.0~SNAPSHOT_<sha>`
    /// rpm. This drives the full production path (yaml → expected_control →
    /// version_matches), unlike `version_matches_normalizes_nfpm_grammar` which
    /// fed a pre-combined expected and so could not catch the dropped field.
    #[test]
    fn expected_control_folds_prerelease_for_snapshot_versions() {
        let yaml = "name: cfgd\nversion: 0.4.0\nprerelease: SNAPSHOT-3d07f6c\narch: amd64\n";
        let deb = expected_control(yaml, "deb").expect("deb expected_control");
        assert_eq!(deb.version, "0.4.0~SNAPSHOT-3d07f6c");
        assert!(
            version_matches("1:0.4.0~SNAPSHOT-3d07f6c-1", &deb.version),
            "built snapshot deb must match the prerelease-folded expected; got {:?}",
            deb.version
        );
        let rpm = expected_control(yaml, "rpm").expect("rpm expected_control");
        assert!(
            version_matches("0.4.0~SNAPSHOT_3d07f6c", &rpm.version),
            "built snapshot rpm must match the prerelease-folded expected; got {:?}",
            rpm.version
        );

        // A release build (no prerelease) is unchanged: expected stays the bare
        // core and still matches the deb epoch/release grammar.
        let rel = expected_control("name: cfgd\nversion: 0.4.0\narch: amd64\n", "deb")
            .expect("release expected_control");
        assert_eq!(rel.version, "0.4.0");
        assert!(version_matches("1:0.4.0-1", &rel.version));
    }

    /// The validator must render the nfpm `version:` under the SAME global
    /// artifact version the build stamped — not the per-crate *bare* tag the
    /// surrounding `with_validated_crate_scope` re-derives. The build's nfpm
    /// stage reads the global `Version` (the snapshot `<base>-SNAPSHOT-<sha>`)
    /// for every crate, while `resolve_crate_tag` yields the bare release tag
    /// (`v0.4.0` → `0.4.0`), so without pinning the version back the validator
    /// renders `0.4.0` against a built `1:0.4.0~SNAPSHOT-<sha>-1` deb and every
    /// snapshot nfpm package spuriously fails the control cross-check (the cfgd
    /// dogfood failure, 2026-06-06).
    ///
    /// This drives the REAL scope path (`with_validated_crate_scope` →
    /// `render_build_matched_nfpm_configs`) with a production-faithful resolver
    /// that returns the bare tag — unlike the other tests, which call
    /// `nfpm_yaml_configs_for_crate` directly or pass
    /// `test_current_version_resolver` (which returns the already-labeled
    /// `ctx.version()` and so masked the bug).
    #[test]
    fn validate_renders_build_artifact_version_not_bare_tag_rederivation() {
        let cfg = NfpmConfig {
            package_name: Some("cfgd".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            ..every_option_nfpm_cfg()
        };
        let krate = nfpm_crate("cfgd", "v{{ .Version }}", cfg);
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("cfgd")
            .crates(vec![krate])
            .build();
        scope_version(&mut ctx, "0.4.0-SNAPSHOT-b348321");
        add_linux_binary(&mut ctx, "cfgd", "cfgd");

        // What `resolve_crate_tag` returns at a tagged HEAD: the bare release
        // tag with the monorepo prefix stripped — no snapshot label.
        let bare_tag = |_: &Context, _: &CrateConfig| Some("0.4.0".to_string());

        let artifact_version = ctx.version();
        let configs = with_validated_crate_scope(&mut ctx, "cfgd", &bare_tag, |ctx| {
            render_build_matched_nfpm_configs(ctx, "cfgd", &artifact_version)
        })
        .expect("render ok");

        assert!(!configs.is_empty(), "expected rendered nfpm configs");
        for c in &configs {
            assert!(
                c.yaml.contains("version: 0.4.0-SNAPSHOT-b348321"),
                "validator must render the build's snapshot version, got:\n{}",
                c.yaml
            );
            assert!(
                !c.yaml.contains("version: 0.4.0\n"),
                "validator must NOT render the bare tag version, got:\n{}",
                c.yaml
            );
        }
    }

    /// Per-crate independent-version mode: each crate's pipeline runs under its
    /// own global `Version` (the orchestration scopes it before the build), so
    /// the validator must stamp each crate's OWN snapshot version with no
    /// cross-crate bleed — even though the bare per-crate tag resolver strips
    /// every snapshot label. Proves the fix in the third config mode the
    /// lockstep cfgd capstone cannot exercise.
    #[test]
    fn validate_per_crate_independent_renders_each_own_snapshot_version() {
        let alpha = nfpm_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            NfpmConfig {
                package_name: Some("alpha".to_string()),
                ..every_option_nfpm_cfg()
            },
        );
        let beta = nfpm_crate(
            "beta",
            "beta-v{{ .Version }}",
            NfpmConfig {
                package_name: Some("beta".to_string()),
                ..every_option_nfpm_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .project_name("alpha")
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "1.2.0-SNAPSHOT-aaaaaaa");
        add_linux_binary(&mut ctx_a, "alpha", "alpha");
        let alpha_tag = |_: &Context, _: &CrateConfig| Some("1.2.0".to_string());
        let av = ctx_a.version();
        let configs_a = with_validated_crate_scope(&mut ctx_a, "alpha", &alpha_tag, |ctx| {
            render_build_matched_nfpm_configs(ctx, "alpha", &av)
        })
        .expect("render ok");
        assert!(
            configs_a
                .iter()
                .all(|c| c.yaml.contains("version: 1.2.0-SNAPSHOT-aaaaaaa")),
            "alpha must stamp its own snapshot version, got: {:?}",
            configs_a.iter().map(|c| &c.yaml).collect::<Vec<_>>()
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .project_name("beta")
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "0.5.0-SNAPSHOT-bbbbbbb");
        add_linux_binary(&mut ctx_b, "beta", "beta");
        let beta_tag = |_: &Context, _: &CrateConfig| Some("0.5.0".to_string());
        let bv = ctx_b.version();
        let configs_b = with_validated_crate_scope(&mut ctx_b, "beta", &beta_tag, |ctx| {
            render_build_matched_nfpm_configs(ctx, "beta", &bv)
        })
        .expect("render ok");
        assert!(
            configs_b
                .iter()
                .all(|c| c.yaml.contains("version: 0.5.0-SNAPSHOT-bbbbbbb")),
            "beta must stamp its own snapshot version (no alpha bleed), got: {:?}",
            configs_b.iter().map(|c| &c.yaml).collect::<Vec<_>>()
        );
    }

    /// (a) Single-crate, every option set: every rendered nfpm config must
    /// conform with zero findings, and the key fields must land in the
    /// schema-expected places.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate nfpm configs must conform, got: {findings:?}"
        );

        let configs =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx, "widget").expect("render ok");
        assert_eq!(configs.len(), 2, "two formats (deb, rpm) → two configs");
        let deb = configs
            .iter()
            .find(|c| c.format == "deb")
            .expect("a deb config");
        let value = yaml_to_json(&deb.yaml).expect("nfpm config is YAML");

        assert_eq!(
            value.pointer("/name").and_then(Value::as_str),
            Some("widget")
        );
        assert_eq!(
            value.pointer("/version").and_then(Value::as_str),
            Some("1.0.0")
        );
        assert_eq!(
            value.pointer("/arch").and_then(Value::as_str),
            Some("amd64"),
            "the x86_64 linux target stamps the amd64 nfpm arch"
        );
        assert_eq!(
            value.pointer("/maintainer").and_then(Value::as_str),
            Some("Acme <ops@acme.example>")
        );
        assert!(
            value.pointer("/contents/0/dst").is_some(),
            "the configured content entry lands in the config"
        );
    }

    /// (b) Workspace-lockstep: multiple crates share one global version. Each
    /// crate's configs must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = nfpm_crate(
            "alpha",
            "v{{ .Version }}",
            NfpmConfig {
                package_name: Some("alpha".to_string()),
                ..every_option_nfpm_cfg()
            },
        );
        let beta = nfpm_crate(
            "beta",
            "v{{ .Version }}",
            NfpmConfig {
                package_name: Some("beta".to_string()),
                ..every_option_nfpm_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("acme")
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "alpha", "alpha");
        add_linux_binary(&mut ctx, "beta", "beta");

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace nfpm configs must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate: each crate carries its own tag_template /
    /// version. The validator (run per-crate via `--crate`) must conform and
    /// stamp each crate's own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = nfpm_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            NfpmConfig {
                package_name: Some("alpha".to_string()),
                ..every_option_nfpm_cfg()
            },
        );
        let beta = nfpm_crate(
            "beta",
            "beta-v{{ .Version }}",
            NfpmConfig {
                package_name: Some("beta".to_string()),
                ..every_option_nfpm_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .project_name("alpha")
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_linux_binary(&mut ctx_a, "alpha", "alpha");
        let findings_a = NfpmSchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let configs_a =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx_a, "alpha").expect("render ok");
        assert!(
            configs_a.iter().all(|c| c.yaml.contains("version: 2.0.0")),
            "alpha config stamps its own version, got: {:?}",
            configs_a.iter().map(|c| &c.yaml).collect::<Vec<_>>()
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .project_name("beta")
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_linux_binary(&mut ctx_b, "beta", "beta");
        let findings_b = NfpmSchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let configs_b =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx_b, "beta").expect("render ok");
        assert!(
            configs_b.iter().all(|c| c.yaml.contains("version: 3.1.0")),
            "beta config stamps its own version, got: {:?}",
            configs_b.iter().map(|c| &c.yaml).collect::<Vec<_>>()
        );
    }

    /// A non-amd64 target must stamp its own nfpm arch — proving anodizer no
    /// longer relies on nfpm's silent `amd64` default for every package.
    #[test]
    fn non_amd64_target_stamps_its_own_arch() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary_on_target(&mut ctx, "widget", "widget", "aarch64-unknown-linux-gnu");

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "aarch64 nfpm configs must conform, got: {findings:?}"
        );
        let configs =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx, "widget").expect("render ok");
        assert!(
            configs.iter().all(|c| c.arch == "arm64"),
            "an aarch64 target stamps the arm64 nfpm arch, got: {:?}",
            configs.iter().map(|c| &c.arch).collect::<Vec<_>>()
        );
    }

    /// `deb.arch_variant` is a real nfpm field (v2.46.3): an amd64 deb whose
    /// artifact carries `amd64_variant` (GOAMD64) metadata must auto-derive
    /// `deb.arch_variant` into the offline YAML — matching the live build's
    /// `fill_deb_arch_variant` — and the resulting config must conform to the
    /// nfpm schema (zero findings).
    #[test]
    fn deb_arch_variant_is_auto_derived_and_config_conforms() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary_with_variant(&mut ctx, "widget", "widget", "v3");

        let configs =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx, "widget").expect("render ok");
        let deb = configs
            .iter()
            .find(|c| c.format == "deb")
            .expect("a deb config");
        let value = yaml_to_json(&deb.yaml).expect("nfpm config is YAML");
        assert_eq!(
            value.pointer("/deb/arch_variant").and_then(Value::as_str),
            Some("v3"),
            "the offline deb YAML must auto-derive arch_variant from artifact \
             metadata (matching the build), got: {}",
            deb.yaml
        );

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "an arch_variant-bearing config must conform to the v2.46.3 schema, \
             got: {findings:?}"
        );
    }

    /// Shard tolerance: a snapshot shard that built no eligible binary for an
    /// nfpm-configured crate must SKIP it (zero findings, no error) rather than
    /// render an empty config.
    #[test]
    fn crate_without_eligible_binary_is_skipped_not_failed() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent binary");
        assert!(
            findings.is_empty(),
            "a crate with no eligible binary in this shard must be skipped, got: {findings:?}"
        );
    }

    /// A falsy `if:` suppresses the config: the renderer yields nothing, so the
    /// validator produces zero findings (no error).
    #[test]
    fn skipped_config_yields_no_findings() {
        let cfg = NfpmConfig {
            if_condition: Some("false".to_string()),
            ..every_option_nfpm_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a falsy-`if` config must be skipped, got: {findings:?}"
        );
        let configs =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx, "widget").expect("render ok");
        assert!(
            configs.is_empty(),
            "the suppressed config renders no nfpm YAML, got: {} configs",
            configs.len()
        );
    }

    /// A registered `LinuxPackage` whose file was never built (the
    /// snapshot/dry-run case: the predicted path exists in `ctx` but no file is
    /// on disk) must not trip the gated layer — the primary schema floor stands
    /// and the validator reports zero findings.
    #[test]
    fn unbuilt_package_artifact_does_not_trip_gated_layer() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        // Register a predicted .deb the build would emit, pointing at a path
        // that does not exist (nothing was actually packaged).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            path: PathBuf::from("/dist/linux/does-not-exist.deb"),
            name: String::new(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: HashMap::from([("format".to_string(), "deb".to_string())]),
            size: None,
        });

        let findings = NfpmSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "an unbuilt predicted package must not trip the gated layer, got: {findings:?}"
        );
    }

    /// Schema layer BITES: a wrong-typed field produces a finding on its
    /// pointer path; the corrected value clears it.
    #[test]
    fn wrong_typed_field_is_reported_then_corrected() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![nfpm_crate(
                "widget",
                "v{{ .Version }}",
                every_option_nfpm_cfg(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        let configs =
            anodizer_stage_nfpm::nfpm_yaml_configs_for_crate(&ctx, "widget").expect("render ok");
        let mut value = yaml_to_json(&configs[0].yaml).expect("config is YAML");

        // `umask` is typed `integer`; a string violates the schema.
        value
            .as_object_mut()
            .expect("config is a map")
            .insert("umask".to_string(), Value::String("not-an-int".to_string()));
        let findings = validate_json("nfpm", &value, NFPM_SCHEMA).expect("validation runs");
        let umask = findings
            .iter()
            .find(|f| f.field == "/umask")
            .unwrap_or_else(|| panic!("a finding for the wrong-typed /umask; got: {findings:?}"));
        assert_eq!(umask.publisher, "nfpm");

        value
            .as_object_mut()
            .expect("config is a map")
            .insert("umask".to_string(), Value::from(18));
        let ok = validate_json("nfpm", &value, NFPM_SCHEMA).expect("validation runs");
        assert!(
            ok.iter().all(|f| f.field != "/umask"),
            "a valid integer umask must conform, got: {ok:?}"
        );
    }

    /// Schema layer BITES on an unknown key: nfpm's schema is
    /// `additionalProperties: false`, so a key it does not define is rejected.
    #[test]
    fn unknown_field_is_reported() {
        let instance = serde_json::json!({
            "name": "widget",
            "arch": "amd64",
            "version": "1.0.0",
            "not_an_nfpm_field": true
        });
        let findings = validate_json("nfpm", &instance, NFPM_SCHEMA).expect("validation runs");
        assert!(
            !findings.is_empty(),
            "an unknown field must be rejected by additionalProperties:false"
        );
    }

    /// The gated control-field layer ACCEPTS a real `.deb` whose control fields
    /// match the config, and BITES when a field is deliberately wrong. Builds
    /// the package with the real `nfpm` CLI. Skipped (visible marker) when
    /// `nfpm` or `dpkg-deb` is absent.
    #[test]
    fn dpkg_deb_control_matches_then_bites() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
        let nfpm_present = anodizer_core::tool_detect::tool_available("nfpm").unwrap_or(false);
        let dpkg_present = anodizer_core::tool_detect::tool_available("dpkg-deb").unwrap_or(false);
        if !nfpm_present || !dpkg_present {
            log.status(&format!(
                "SKIP dpkg_deb_control_matches_then_bites: nfpm={nfpm_present} \
                 dpkg-deb={dpkg_present} (gated .deb layer unexercised)"
            ));
            return;
        }

        // Build an arm64 .deb: the generic nfpm arch `arm64` stays `arm64` in
        // deb nomenclature, proving the translation is exercised end-to-end.
        let (yaml, deb_path, _dir) = build_real_package("deb", "widget", "2.3.4", "arm64");
        let expected = expected_control(&yaml, "deb").expect("expected control");
        assert_eq!(expected.arch, "arm64", "deb keeps the arm64 control arch");

        let ok = check_deb_control(&deb_path, &expected).expect("dpkg-deb runs");
        assert!(
            ok.is_empty(),
            "a matching .deb must report zero findings, got: {ok:?}"
        );

        // Deliberately wrong expected version → the control field no longer
        // matches, so the layer must bite.
        let wrong_version = ExpectedControl {
            version: "9.9.9".to_string(),
            ..clone_expected(&expected)
        };
        let bad = check_deb_control(&deb_path, &wrong_version).expect("dpkg-deb runs");
        assert!(
            bad.iter().any(|f| f.field == "deb:Version"),
            "a version mismatch must bite, got: {bad:?}"
        );

        // Deliberately wrong expected architecture → the built arm64 .deb no
        // longer matches, so the arch-regression guard must bite. This is the
        // exact regression of the arch-mislabel bug this task fixed.
        let wrong_arch = ExpectedControl {
            arch: "amd64".to_string(),
            ..clone_expected(&expected)
        };
        let bad_arch = check_deb_control(&deb_path, &wrong_arch).expect("dpkg-deb runs");
        assert!(
            bad_arch.iter().any(|f| f.field == "deb:Architecture"),
            "an architecture mismatch must bite, got: {bad_arch:?}"
        );
    }

    /// The gated control-field layer ACCEPTS a real `.rpm` whose header matches
    /// and BITES on a deliberate mismatch. Skipped (visible marker) when `nfpm`
    /// or `rpm` is absent.
    #[test]
    fn rpm_control_matches_then_bites() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
        let nfpm_present = anodizer_core::tool_detect::tool_available("nfpm").unwrap_or(false);
        let rpm_present = anodizer_core::tool_detect::tool_available("rpm").unwrap_or(false);
        if !nfpm_present || !rpm_present {
            log.status(&format!(
                "SKIP rpm_control_matches_then_bites: nfpm={nfpm_present} \
                 rpm={rpm_present} (gated .rpm layer unexercised)"
            ));
            return;
        }

        // Build an arm64 .rpm: the generic nfpm arch `arm64` translates to
        // `aarch64` in rpm nomenclature — the validator must mirror that
        // translation, not compare the generic arch.
        let (yaml, rpm_path, _dir) = build_real_package("rpm", "widget", "2.3.4", "arm64");
        let expected = expected_control(&yaml, "rpm").expect("expected control");
        assert_eq!(
            expected.arch, "aarch64",
            "rpm translates the arm64 generic arch to aarch64"
        );

        let ok = check_rpm_control(&rpm_path, &expected).expect("rpm runs");
        assert!(
            ok.is_empty(),
            "a matching .rpm must report zero findings, got: {ok:?}"
        );

        let wrong_name = ExpectedControl {
            name: Some("not-widget".to_string()),
            ..clone_expected(&expected)
        };
        let bad = check_rpm_control(&rpm_path, &wrong_name).expect("rpm runs");
        assert!(
            bad.iter().any(|f| f.field == "rpm:Package"),
            "a package-name mismatch must bite, got: {bad:?}"
        );

        // Deliberately wrong expected architecture → the built aarch64 .rpm no
        // longer matches, so the arch-regression guard must bite.
        let wrong_arch = ExpectedControl {
            arch: "x86_64".to_string(),
            ..clone_expected(&expected)
        };
        let bad_arch = check_rpm_control(&rpm_path, &wrong_arch).expect("rpm runs");
        assert!(
            bad_arch.iter().any(|f| f.field == "rpm:Architecture"),
            "an architecture mismatch must bite, got: {bad_arch:?}"
        );
    }

    fn clone_expected(e: &ExpectedControl) -> ExpectedControl {
        ExpectedControl {
            name: e.name.clone(),
            version: e.version.clone(),
            arch: e.arch.clone(),
            maintainer: e.maintainer.clone(),
        }
    }

    /// Generate a real nfpm config and build a real package via the `nfpm` CLI,
    /// returning the YAML, the built package path, and the owning tempdir
    /// (kept alive by the caller). Used only by the gated tests, which already
    /// verified `nfpm` is present.
    /// `arch` is the GENERIC nfpm architecture (`amd64`, `arm64`, …) stamped
    /// into the YAML `arch:` field; nfpm translates it to the packager's
    /// control nomenclature in the built package, exactly the translation the
    /// validator must mirror.
    fn build_real_package(
        format: &str,
        name: &str,
        version: &str,
        arch: &str,
    ) -> (String, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin_path = dir.path().join(name);
        std::fs::write(&bin_path, b"#!/bin/sh\necho widget\n").expect("write fake binary");

        let cfg = NfpmConfig {
            package_name: Some(name.to_string()),
            formats: vec![format.to_string()],
            maintainer: Some("Acme <ops@acme.example>".to_string()),
            description: Some("A widget".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let yaml = anodizer_stage_nfpm::generate_nfpm_yaml(
            &cfg,
            version,
            &[bin_path.to_string_lossy().into_owned()],
            Some(format),
            true,
            &anodizer_stage_nfpm::NfpmLibraryPaths::default(),
        )
        .expect("generate nfpm yaml");
        // generate_nfpm_yaml defaults the generic arch to amd64; rewrite to the
        // requested generic arch so nfpm performs its real per-format
        // translation when building the package.
        let yaml = yaml.replace("arch: amd64", &format!("arch: {arch}"));

        let cfg_path = dir.path().join("nfpm.yaml");
        std::fs::write(&cfg_path, &yaml).expect("write nfpm config");
        let pkg_path = dir.path().join(format!("{name}.{format}"));

        let args = anodizer_stage_nfpm::nfpm_command(
            &cfg_path.to_string_lossy(),
            format,
            &pkg_path.to_string_lossy(),
        );
        let output = Command::new(&args[0])
            .args(&args[1..])
            .output()
            .expect("run nfpm");
        assert!(
            output.status.success(),
            "nfpm pkg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (yaml, pkg_path, dir)
    }

    /// A real nfpm v2.46.3 build of a deb config that sets `deb.arch_variant`
    /// must SUCCEED — proving `arch_variant` is a field the installed nfpm
    /// accepts (nfpm strict-errors on an unknown deb key, the way it rejects
    /// `meta` / `lintian_overrides`). Skipped (visible marker) when `nfpm` is
    /// absent.
    #[test]
    fn nfpm_builds_deb_with_arch_variant() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Normal);
        if !anodizer_core::tool_detect::tool_available("nfpm").unwrap_or(false) {
            log.status("SKIP nfpm_builds_deb_with_arch_variant: nfpm not on PATH");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let bin_path = dir.path().join("widget");
        std::fs::write(&bin_path, b"#!/bin/sh\necho widget\n").expect("write fake binary");

        // Minimal config so the only deb-specific field under test is
        // `arch_variant`; the auto-emitted binary content entry points at the
        // real on-disk binary so nfpm can pack it.
        let cfg = NfpmConfig {
            package_name: Some("widget".to_string()),
            formats: vec!["deb".to_string()],
            maintainer: Some("Acme <ops@acme.example>".to_string()),
            description: Some("A widget".to_string()),
            license: Some("MIT".to_string()),
            deb: Some(NfpmDebConfig {
                compression: Some("xz".to_string()),
                arch_variant: Some("v3".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = anodizer_stage_nfpm::generate_nfpm_yaml(
            &cfg,
            "2.3.4",
            &[bin_path.to_string_lossy().into_owned()],
            Some("deb"),
            true,
            &anodizer_stage_nfpm::NfpmLibraryPaths::default(),
        )
        .expect("generate nfpm yaml");
        assert!(
            yaml.contains("arch_variant: v3"),
            "the generated deb YAML must carry arch_variant, got:\n{yaml}"
        );

        // The generated config must also pass the offline schema layer.
        let value = yaml_to_json(&yaml).expect("config is YAML");
        let findings = validate_json("nfpm", &value, NFPM_SCHEMA).expect("validation runs");
        assert!(
            findings.is_empty(),
            "the arch_variant config must conform to the v2.46.3 schema, got: {findings:?}"
        );

        let cfg_path = dir.path().join("nfpm.yaml");
        std::fs::write(&cfg_path, &yaml).expect("write nfpm config");
        let pkg_path = dir.path().join("widget.deb");
        let args = anodizer_stage_nfpm::nfpm_command(
            &cfg_path.to_string_lossy(),
            "deb",
            &pkg_path.to_string_lossy(),
        );
        let output = Command::new(&args[0])
            .args(&args[1..])
            .output()
            .expect("run nfpm");
        assert!(
            output.status.success(),
            "nfpm must build a deb with arch_variant set, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(pkg_path.exists(), "the .deb must be produced");
    }
}
