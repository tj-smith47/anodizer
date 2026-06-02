//! Snap metadata (`snap.yaml`) schema validation.
//!
//! The Snap Store / snapd validates the final `snap.yaml` metadata a snap
//! carries (its name, version, summary, confinement, grade, architectures,
//! apps, …) when a `.snap` is uploaded — and it does so in Go, shipping no
//! standalone JSON Schema. anodizer primes directly and writes that metadata
//! to `prime/meta/snap.yaml` per crate per target; this validator renders the
//! exact YAML the live build stages — via the same render path — and checks
//! it against a schema transcribed from snapd's own validators, so a
//! structural defect (an over-long summary, an out-of-enum confinement, a
//! malformed name) surfaces in the snapshot/dry-run pass rather than after a
//! release has packed and uploaded a registry-rejected snap.
//!
//! This validates `snap.yaml` (the snap's final metadata), NOT a
//! `snapcraft.yaml` build recipe — anodizer does not emit a recipe, and the
//! two documents have different schemas.

use anodizer_core::context::Context;
use anyhow::Result;

use super::{PublisherSchemaValidator, SchemaFinding, validate_json, yaml_to_json};

/// The snap.yaml metadata schema (draft 2020-12), authored from snapd's own
/// Go validators (`snap/validate.go` / `snap/info.go`) plus the snap-format
/// docs. Pinned and embedded so validation is fully offline; refresh via
/// `schemas/SOURCES.md`.
const SNAPCRAFT_SNAP_YAML_SCHEMA: &str =
    include_str!("../../schemas/snapcraft.snap-yaml.schema.json");

/// Validates anodizer's rendered `snap.yaml` metadata against the snap-format
/// schema transcribed from snapd's `ValidateName` / `ValidateVersion` /
/// `ValidateSummary` / `ValidateApp` rules.
pub(crate) struct SnapcraftSchemaValidator;

/// True iff the crate carries at least one non-empty snapcraft config — the
/// same universe the build's `run` loop iterates (`c.snapcrafts.is_some()`,
/// where an empty list yields no snaps).
fn is_snapcraft_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .find(|c| c.name == crate_name)
        .and_then(|c| c.snapcrafts)
        .is_some_and(|cfgs| !cfgs.is_empty())
}

impl PublisherSchemaValidator for SnapcraftSchemaValidator {
    fn publisher(&self) -> &'static str {
        "snapcraft"
    }

    fn validate(&self, ctx: &Context) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk the snapcraft-configured crates (honoring `--crate` selection,
        // else every snapcraft-configured crate) so the validated set equals
        // the built set. Both the build's `run` and the offline renderer
        // resolve a crate via `ctx.config.crates`, so a snapcraft block living
        // only under `workspaces[].crates` is built by neither and validated by
        // neither — the two sets stay identical precisely because both
        // intentionally exclude workspace-only crates.
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_snapcraft_per_crate_configured,
        );
        for crate_name in &selected {
            if !is_snapcraft_per_crate_configured(ctx, crate_name) {
                continue;
            }

            // The render walk returns one snap.yaml per (config, target). An
            // empty Vec means there is nothing to validate — the crate's
            // configs were all `skip:`/`if:`-suppressed, or no Linux binary was
            // built for it in this snapshot shard (the same shard-tolerance
            // case the build's "no Linux binaries → skip" guard hits).
            let yamls = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(ctx, crate_name)?;
            if yamls.is_empty() {
                log.verbose(&format!(
                    "snapcraft: crate '{}' produced no snap.yaml in this snapshot \
                     shard (skipped or no Linux binary); skipping schema validation",
                    crate_name
                ));
                continue;
            }

            for yaml in &yamls {
                let value = yaml_to_json(yaml)?;
                findings.extend(validate_json(
                    "snapcraft",
                    &value,
                    SNAPCRAFT_SNAP_YAML_SCHEMA,
                )?);
            }
        }

        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::ReleaseConfig;
    use anodizer_core::config::{
        CrateConfig, PublishConfig, ScmRepoConfig, SnapcraftApp, SnapcraftConfig, SnapcraftLayout,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;
    use serde_json::Value;

    use super::*;

    /// A `SnapcraftConfig` exercising every snap-affecting option, with values
    /// the snap.yaml schema accepts (valid name, in-enum confinement / grade,
    /// short summary, one app with a command + daemon).
    fn every_option_snap_cfg() -> SnapcraftConfig {
        let mut apps = BTreeMap::new();
        apps.insert(
            "widget".to_string(),
            SnapcraftApp {
                command: Some("widget".to_string()),
                daemon: Some("simple".to_string()),
                plugs: Some(vec!["network".to_string()]),
                slots: Some(vec!["dbus-svc".to_string()]),
                ..Default::default()
            },
        );
        let mut layouts = BTreeMap::new();
        layouts.insert(
            "/var/lib/widget".to_string(),
            SnapcraftLayout {
                bind: Some("$SNAP_DATA/var/lib/widget".to_string()),
                ..Default::default()
            },
        );
        let mut hooks = BTreeMap::new();
        hooks.insert("configure".to_string(), Value::Null);

        SnapcraftConfig {
            name: Some("widget".to_string()),
            title: Some("Widget".to_string()),
            summary: Some("A widget management tool".to_string()),
            description: Some("Manage widgets from the command line.".to_string()),
            base: Some("core22".to_string()),
            grade: Some("stable".to_string()),
            confinement: Some("strict".to_string()),
            license: Some("MIT".to_string()),
            assumes: Some(vec!["snapd2.55".to_string()]),
            plugs: Some(BTreeMap::new()),
            apps: Some(apps),
            layouts: Some(layouts),
            hooks: Some(hooks),
            ..Default::default()
        }
    }

    fn snap_crate(crate_name: &str, tag_template: &str, cfg: SnapcraftConfig) -> CrateConfig {
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
            snapcrafts: Some(vec![cfg]),
            ..Default::default()
        }
    }

    /// Add a Linux binary artifact whose filename drives the snap's command /
    /// default app and whose `x86_64` target drives the `architectures:` field.
    fn add_linux_binary(ctx: &mut Context, crate_name: &str, binary: &str) {
        add_linux_binary_on_target(ctx, crate_name, binary, "x86_64-unknown-linux-gnu");
    }

    /// Add a Linux binary artifact on an explicit target triple — used to
    /// exercise the snap-arch gate with a store-unsupported architecture.
    fn add_linux_binary_on_target(ctx: &mut Context, crate_name: &str, binary: &str, target: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: std::path::PathBuf::from(format!("/dist/{binary}")),
            name: binary.to_string(),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    /// Re-scope the global template vars to the version a release would stamp,
    /// the same shape the publish/build stage applies before invoking a
    /// per-crate stage.
    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    /// Render the every-option snap.yaml a release would ship for `widget`
    /// @ 1.0.0 (one amd64 Linux target) through the same public walker the
    /// validator uses, returning the raw string for a negative test to mutate.
    fn base_widget_snap_yaml() -> String {
        let cfg = every_option_snap_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![snap_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");
        anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx, "widget")
            .expect("render ok")
            .into_iter()
            .next()
            .expect("one snap.yaml")
    }

    /// (a) Single-crate mode: one crate, one tag. Every snap-affecting option
    /// set; the rendered snap.yaml must conform with zero findings and land
    /// each option in the schema-expected field.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_snap_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![snap_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        let findings = SnapcraftSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate snap.yaml must conform, got: {findings:?}"
        );

        let yamls = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx, "widget")
            .expect("render ok");
        assert_eq!(yamls.len(), 1, "one target → one snap.yaml");
        let value = yaml_to_json(&yamls[0]).expect("snap.yaml is YAML");

        assert_eq!(
            value.pointer("/name").and_then(|v| v.as_str()),
            Some("widget")
        );
        assert_eq!(
            value.pointer("/version").and_then(|v| v.as_str()),
            Some("1.0.0")
        );
        assert_eq!(
            value.pointer("/summary").and_then(|v| v.as_str()),
            Some("A widget management tool")
        );
        assert_eq!(
            value.pointer("/confinement").and_then(|v| v.as_str()),
            Some("strict")
        );
        assert_eq!(
            value.pointer("/grade").and_then(|v| v.as_str()),
            Some("stable")
        );
        assert_eq!(
            value.pointer("/architectures/0").and_then(|v| v.as_str()),
            Some("amd64"),
            "the x86_64 linux target maps to the amd64 snap arch"
        );
        assert_eq!(
            value
                .pointer("/apps/widget/command")
                .and_then(|v| v.as_str()),
            Some("widget")
        );
        assert_eq!(
            value
                .pointer("/apps/widget/daemon")
                .and_then(|v| v.as_str()),
            Some("simple")
        );
    }

    /// (b) Workspace-lockstep mode: multiple crates share one version/tag. Each
    /// crate's snap.yaml must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = snap_crate(
            "alpha",
            "v{{ .Version }}",
            SnapcraftConfig {
                name: Some("alpha".to_string()),
                ..every_option_snap_cfg()
            },
        );
        let beta = snap_crate(
            "beta",
            "v{{ .Version }}",
            SnapcraftConfig {
                name: Some("beta".to_string()),
                ..every_option_snap_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("acme")
            .crates(vec![alpha, beta])
            .build();
        // Lockstep: a single global version names every crate's snap.
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "alpha", "alpha");
        add_linux_binary(&mut ctx, "beta", "beta");

        let findings = SnapcraftSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace snap.yamls must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate mode: each crate carries its own tag_template /
    /// version. The publish/build stage scopes the global `Version` to the
    /// per-crate value before invoking the stage, so the validator (run
    /// per-crate via `--crate`) must conform — and stamp — under each crate's
    /// own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = snap_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            SnapcraftConfig {
                name: Some("alpha".to_string()),
                ..every_option_snap_cfg()
            },
        );
        let beta = snap_crate(
            "beta",
            "beta-v{{ .Version }}",
            SnapcraftConfig {
                name: Some("beta".to_string()),
                ..every_option_snap_cfg()
            },
        );

        // alpha @ 2.0.0
        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .project_name("alpha")
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_linux_binary(&mut ctx_a, "alpha", "alpha");
        let findings_a = SnapcraftSchemaValidator
            .validate(&ctx_a)
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let yamls_a = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx_a, "alpha")
            .expect("render ok");
        assert!(
            yamls_a[0].contains("version: 2.0.0"),
            "alpha snap.yaml stamps its own version, got: {}",
            yamls_a[0]
        );

        // beta @ 3.1.0 — its own version stamps its own snap.yaml.
        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .project_name("beta")
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_linux_binary(&mut ctx_b, "beta", "beta");
        let findings_b = SnapcraftSchemaValidator
            .validate(&ctx_b)
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let yamls_b = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx_b, "beta")
            .expect("render ok");
        assert!(
            yamls_b[0].contains("version: 3.1.0"),
            "beta snap.yaml stamps its own version, got: {}",
            yamls_b[0]
        );
    }

    /// A single-target / sharded snapshot that built no Linux binary for a
    /// snapcraft-configured crate must SKIP it (zero findings, no error)
    /// rather than render an empty snap — there is nothing to validate. This
    /// is the exact case anodizer's own non-linux snapshot shard would hit.
    #[test]
    fn crate_without_linux_binary_is_skipped_not_failed() {
        let cfg = every_option_snap_cfg();
        let ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![snap_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        // No Linux binary artifact in this shard at all.
        let findings = SnapcraftSchemaValidator
            .validate(&ctx)
            .expect("validation runs without erroring on the absent binary");
        assert!(
            findings.is_empty(),
            "a crate with no Linux binary in this shard must be skipped, got: {findings:?}"
        );
    }

    /// The offline walk must apply the build's per-target snap-arch gate: a
    /// crate whose only Linux binary targets an arch the snap store does not
    /// support (riscv64 maps to a snap arch outside `is_valid_snap_arch`) is
    /// not staged by the build, so the validator must render no snap.yaml for
    /// it — keeping the validated (target → snap) set byte-identical to the
    /// built set.
    #[test]
    fn crate_with_only_invalid_snap_arch_target_is_skipped() {
        let cfg = every_option_snap_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![snap_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // riscv64 is a Linux target but snapd's store does not list its snap
        // arch, so the build's `is_valid_snap_arch` gate drops it.
        add_linux_binary_on_target(&mut ctx, "widget", "widget", "riscv64gc-unknown-linux-gnu");

        let yamls = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx, "widget")
            .expect("render ok");
        assert!(
            yamls.is_empty(),
            "an invalid-snap-arch-only crate renders no snap.yaml, got: {yamls:?}"
        );
        let findings = SnapcraftSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "an invalid-snap-arch-only crate must yield zero findings, got: {findings:?}"
        );
    }

    /// A falsy `if:` / truthy `skip:` suppresses the config: the renderer
    /// returns nothing, so the validator yields zero findings (no error).
    #[test]
    fn skipped_config_yields_no_findings() {
        let cfg = SnapcraftConfig {
            if_condition: Some("false".to_string()),
            ..every_option_snap_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .crates(vec![snap_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_binary(&mut ctx, "widget", "widget");

        let findings = SnapcraftSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a falsy-`if` config must be skipped, got: {findings:?}"
        );
        let yamls = anodizer_stage_snapcraft::snapcraft_snap_yamls_for_crate(&ctx, "widget")
            .expect("render ok");
        assert!(
            yamls.is_empty(),
            "the suppressed config renders no snap.yaml, got: {yamls:?}"
        );
    }

    /// The check must BITE on the headline rule: snapd caps `summary` at 78
    /// characters (`ValidateSummary`). The renderer truncates to 78 so this
    /// never reaches the schema from real output — feed an over-long summary
    /// directly to prove the rule rejects it, then show the corrected value
    /// conforms.
    #[test]
    fn over_long_summary_is_reported() {
        let base = yaml_to_json(&base_widget_snap_yaml()).expect("snap.yaml is YAML");

        // 79 chars — one over the snapd cap.
        let over_long = "x".repeat(79);
        let mut bad = base.clone();
        bad.as_object_mut()
            .expect("snap.yaml is a map")
            .insert("summary".to_string(), Value::String(over_long));
        let findings =
            validate_json("snapcraft", &bad, SNAPCRAFT_SNAP_YAML_SCHEMA).expect("validation runs");
        let summary_finding = findings
            .iter()
            .find(|f| f.field == "/summary")
            .unwrap_or_else(|| panic!("a finding for the 79-char summary; got: {findings:?}"));
        assert_eq!(summary_finding.publisher, "snapcraft");

        // 78 chars — exactly at the cap — conforms.
        let mut good = base;
        good.as_object_mut()
            .expect("snap.yaml is a map")
            .insert("summary".to_string(), Value::String("y".repeat(78)));
        let ok =
            validate_json("snapcraft", &good, SNAPCRAFT_SNAP_YAML_SCHEMA).expect("validation runs");
        assert!(
            ok.iter().all(|f| f.field != "/summary"),
            "a 78-char summary must conform, got: {ok:?}"
        );
    }

    /// An out-of-enum `confinement` bites: snapd accepts only
    /// strict/devmode/classic. Show the corrected value conforms.
    #[test]
    fn invalid_confinement_is_reported() {
        let mut value = yaml_to_json(&base_widget_snap_yaml()).expect("snap.yaml is YAML");

        value.as_object_mut().expect("snap.yaml is a map").insert(
            "confinement".to_string(),
            Value::String("looose".to_string()),
        );
        let findings = validate_json("snapcraft", &value, SNAPCRAFT_SNAP_YAML_SCHEMA)
            .expect("validation runs");
        let finding = findings
            .iter()
            .find(|f| f.field == "/confinement")
            .unwrap_or_else(|| panic!("a finding for the bad confinement; got: {findings:?}"));
        assert_eq!(finding.publisher, "snapcraft");

        // The corrected value conforms.
        value.as_object_mut().expect("snap.yaml is a map").insert(
            "confinement".to_string(),
            Value::String("classic".to_string()),
        );
        let ok = validate_json("snapcraft", &value, SNAPCRAFT_SNAP_YAML_SCHEMA)
            .expect("validation runs");
        assert!(
            ok.iter().all(|f| f.field != "/confinement"),
            "a valid confinement must conform, got: {ok:?}"
        );
    }

    /// An invalid `name` bites: snapd's `ValidateName` rejects uppercase and
    /// underscores (the pattern admits only lowercase alphanumerics + single
    /// hyphens). Show the corrected value conforms.
    #[test]
    fn invalid_name_is_reported() {
        let mut value = yaml_to_json(&base_widget_snap_yaml()).expect("snap.yaml is YAML");

        value
            .as_object_mut()
            .expect("snap.yaml is a map")
            .insert("name".to_string(), Value::String("Bad_Name".to_string()));
        let findings = validate_json("snapcraft", &value, SNAPCRAFT_SNAP_YAML_SCHEMA)
            .expect("validation runs");
        let finding = findings
            .iter()
            .find(|f| f.field == "/name")
            .unwrap_or_else(|| panic!("a finding for the bad name; got: {findings:?}"));
        assert_eq!(finding.publisher, "snapcraft");

        // The corrected value conforms.
        value
            .as_object_mut()
            .expect("snap.yaml is a map")
            .insert("name".to_string(), Value::String("good-name".to_string()));
        let ok = validate_json("snapcraft", &value, SNAPCRAFT_SNAP_YAML_SCHEMA)
            .expect("validation runs");
        assert!(
            ok.iter().all(|f| f.field != "/name"),
            "a valid name must conform, got: {ok:?}"
        );
    }
}
