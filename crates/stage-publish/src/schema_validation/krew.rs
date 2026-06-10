//! Krew plugin-manifest schema validation.
//!
//! krew-index CI validates every submitted plugin manifest against the
//! structural rules krew's `ValidatePlugin` enforces in Go: a supported
//! `apiVersion`, the `Plugin` kind, a safe `metadata.name`, a semver
//! `spec.version`, a non-empty single-line `spec.shortDescription`, and at
//! least one `spec.platforms[]` each carrying a `selector`, a 64-hex `sha256`,
//! a `uri`, and a `bin`. anodizer renders that manifest per crate; this
//! validator renders the exact YAML the live publish would submit — via the
//! same render path — and checks it against the vendored schema, so a
//! structural defect (a non-semver version, a malformed sha256, a missing
//! selector) surfaces in the snapshot/dry-run pass rather than after a
//! krew-index PR has already been opened.

use anodizer_core::context::Context;
use anyhow::Result;

use super::{
    PublisherSchemaValidator, SchemaFinding, TagResolver, validate_json,
    with_validated_crate_scope, yaml_to_json,
};
use crate::krew::{
    crate_has_krew_artifacts, is_krew_per_crate_configured, render_krew_manifest_for_crate,
};

/// The krew plugin-manifest schema (draft 2020-12), authored from krew's own
/// Go validators (`validate.go` / `types.go`). Pinned and embedded so
/// validation is fully offline; refresh via `schemas/SOURCES.md`.
const KREW_SCHEMA: &str = include_str!("../../schemas/krew.v1alpha2.schema.json");

/// Validates anodizer's rendered krew plugin manifests against the krew
/// v1alpha2 schema transcribed from krew's `ValidatePlugin` rules.
pub(crate) struct KrewSchemaValidator;

impl PublisherSchemaValidator for KrewSchemaValidator {
    fn publisher(&self) -> &'static str {
        "krew"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk exactly the crate set the live krew publisher's `run` iterates
        // (honoring `--crate` selection, else every krew-configured crate) so
        // the validated set equals the published set in all config modes.
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_krew_per_crate_configured);
        for crate_name in &selected {
            if !is_krew_per_crate_configured(ctx, crate_name) {
                continue;
            }
            // A real release always produces at least one archive artifact (the
            // publish path errors otherwise), but a single-target / sharded
            // snapshot may build only one platform. Skip a crate whose archives
            // were not built in this run rather than fail on the publisher's own
            // "no archive artifacts" guard — there is nothing to render or
            // validate. The eligibility predicate is the SAME collector the live
            // publish uses, so the skipped set never diverges.
            let Some(krew_cfg) = crate::util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.krew)
            else {
                continue;
            };
            if !crate_has_krew_artifacts(ctx, crate_name, &krew_cfg)? {
                log.verbose(&format!(
                    "krew: crate '{}' produced no archive artifact in this \
                     snapshot shard; skipping schema validation",
                    crate_name
                ));
                continue;
            }

            // Render + validate under THIS crate's own version (workspace
            // per-crate independent-version mode renders each crate's manifest
            // against its own version, not the first crate's).
            let crate_findings = with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                // `None` means the publisher would skip this crate
                // (skip / skip_upload / falsy `if`) — nothing to validate.
                let Some(manifest) = render_krew_manifest_for_crate(ctx, crate_name, &log)? else {
                    return Ok(Vec::new());
                };
                let value = yaml_to_json(&manifest)?;
                validate_json("krew", &value, KREW_SCHEMA)
            })?;
            findings.extend(crate_findings);
        }

        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::ReleaseConfig;
    use anodizer_core::config::{
        CrateConfig, KrewConfig, PublishConfig, RepositoryConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;
    use crate::krew::render_krew_manifest_for_crate;

    /// A `KrewConfig` exercising every manifest-affecting option, with values
    /// the krew schema accepts (URL homepage, single-line short description).
    fn every_option_krew_cfg() -> KrewConfig {
        KrewConfig {
            name: Some("kubectl-widget".to_string()),
            ids: None,
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("krew-index".to_string()),
                ..Default::default()
            }),
            commit_author: None,
            commit_msg_template: Some("krew: {{ ProjectName }} {{ Tag }}".to_string()),
            description: Some("A widget management kubectl plugin.".to_string()),
            short_description: Some("Manage widgets from kubectl".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            url_template: Some(
                "https://dl.acme.example/{{ name }}/{{ version }}/{{ os }}-{{ arch }}.tar.gz"
                    .to_string(),
            ),
            caveats: Some("Run `kubectl widget --help` to get started.".to_string()),
            skip_upload: None,
            skip: None,
            amd64_variant: Some("v1".to_string()),
            arm_variant: Some("7".to_string()),
            update_existing_pr: None,
            required: Some(true),
            if_condition: None,
            mode: None,
            retain_on_rollback: None,
        }
    }

    fn krew_crate(crate_name: &str, tag_template: &str, cfg: KrewConfig) -> CrateConfig {
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
            publish: Some(PublishConfig {
                krew: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Add a Linux archive (with the sha256 + url metadata the manifest's
    /// `platforms[]` block needs) plus the in-archive binary name that drives
    /// the `bin` entry.
    fn add_linux_archive(ctx: &mut Context, crate_name: &str, binary: &str) {
        add_archive_with_id(ctx, crate_name, None, binary);
    }

    /// Add one Linux archive carrying explicit `id` (when `Some`) and `binary`
    /// metadata — the shape the `ids` filter and the `bin`-name collector key
    /// off.
    fn add_archive_with_id(ctx: &mut Context, crate_name: &str, id: Option<&str>, binary: &str) {
        let target = "x86_64-unknown-linux-gnu";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{target}.tar.gz"
            ),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("format".to_string(), "tar.gz".to_string());
        // The one-binary-per-archive check reads `extra_binaries`, and the
        // manifest `bin:` is the archive's first extra binary — so a single
        // entry here both passes the count guard and names the platform `bin`.
        meta.insert("extra_binaries".to_string(), binary.to_string());
        if let Some(id) = id {
            meta.insert("id".to_string(), id.to_string());
        }
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{binary}-{target}.tar.gz")),
            name: format!("{binary}-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    /// Re-scope the global template vars to the version a release would stamp,
    /// the same shape `with_crate_scope` applies before the publish stage
    /// invokes a per-crate publisher.
    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    /// (a) Single-crate mode: one crate, one tag. Every exposed option set; the
    /// rendered manifest must conform with zero findings and land each option in
    /// the krew-expected field.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "kubectl-widget");

        let findings = KrewSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate manifest must conform, got: {findings:?}"
        );

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let value = yaml_to_json(&manifest).expect("manifest is YAML");

        assert_eq!(
            value.pointer("/metadata/name").and_then(|v| v.as_str()),
            Some("kubectl-widget"),
            "the krew.name override lands in metadata.name"
        );
        assert_eq!(
            value.pointer("/spec/version").and_then(|v| v.as_str()),
            Some("v1.0.0"),
            "spec.version carries the v-prefixed semver"
        );
        assert_eq!(
            value
                .pointer("/spec/shortDescription")
                .and_then(|v| v.as_str()),
            Some("Manage widgets from kubectl")
        );
        assert_eq!(
            value.pointer("/spec/homepage").and_then(|v| v.as_str()),
            Some("https://acme.example/widget")
        );
        assert!(
            value
                .pointer("/spec/caveats")
                .and_then(|v| v.as_str())
                .is_some_and(|c| c.contains("kubectl widget --help")),
            "caveats lands under spec.caveats"
        );
        // The Linux archive lands as a platform entry with its selector,
        // uri, sha256, and bin.
        let os = value
            .pointer("/spec/platforms/0/selector/matchLabels/os")
            .and_then(|v| v.as_str());
        assert_eq!(os, Some("linux"));
        let arch = value
            .pointer("/spec/platforms/0/selector/matchLabels/arch")
            .and_then(|v| v.as_str());
        assert_eq!(arch, Some("amd64"));
        // The `url_template` rewrites `platforms[].uri` (not the raw artifact
        // URL), with {{ name }}/{{ version }}/{{ os }}-{{ arch }} substituted.
        assert_eq!(
            value
                .pointer("/spec/platforms/0/uri")
                .and_then(|v| v.as_str()),
            Some("https://dl.acme.example/widget/1.0.0/linux-amd64.tar.gz"),
            "url_template rewrites the platform uri"
        );
        assert!(
            value
                .pointer("/spec/platforms/0/sha256")
                .and_then(|v| v.as_str())
                .is_some_and(|h| h.len() == 64),
            "sha256 lands under the platform entry"
        );
        assert_eq!(
            value
                .pointer("/spec/platforms/0/bin")
                .and_then(|v| v.as_str()),
            Some("kubectl-widget")
        );
    }

    /// (b) Workspace-lockstep mode: multiple crates share one version/tag. Each
    /// crate's manifest must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = krew_crate(
            "alpha",
            "v{{ .Version }}",
            KrewConfig {
                name: Some("kubectl-alpha".to_string()),
                ..every_option_krew_cfg()
            },
        );
        let beta = krew_crate(
            "beta",
            "v{{ .Version }}",
            KrewConfig {
                name: Some("kubectl-beta".to_string()),
                ..every_option_krew_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        // Lockstep: a single global version names every crate's archives.
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "alpha", "kubectl-alpha");
        add_linux_archive(&mut ctx, "beta", "kubectl-beta");

        let findings = KrewSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace manifests must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate mode: each crate carries its own tag_template /
    /// version. The publish stage scopes the global `Version` to the per-crate
    /// value before invoking the publisher, so the validator (run per-crate via
    /// `--crate`) must conform under each crate's own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = krew_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            KrewConfig {
                name: Some("kubectl-alpha".to_string()),
                ..every_option_krew_cfg()
            },
        );
        let beta = krew_crate(
            "beta",
            "beta-v{{ .Version }}",
            KrewConfig {
                name: Some("kubectl-beta".to_string()),
                ..every_option_krew_cfg()
            },
        );

        // alpha @ 2.0.0
        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_linux_archive(&mut ctx_a, "alpha", "kubectl-alpha");
        let findings_a = KrewSchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let manifest_a = render_krew_manifest_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest_a.contains("version: v2.0.0"),
            "alpha manifest stamps its own version, got: {manifest_a}"
        );

        // beta @ 3.1.0 — its own version stamps its own manifest.
        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_linux_archive(&mut ctx_b, "beta", "kubectl-beta");
        let findings_b = KrewSchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let manifest_b = render_krew_manifest_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest_b.contains("version: v3.1.0"),
            "beta manifest stamps its own version, got: {manifest_b}"
        );
    }

    /// A single-target / sharded snapshot that built no archive for a
    /// krew-configured crate must SKIP it (zero findings, no error) rather than
    /// trip the publisher's "no archive artifacts" guard — there is nothing to
    /// render or validate. This is the exact case anodizer's own linux-only
    /// `task snapshot` would hit for a crate whose archives land on another
    /// shard.
    #[test]
    fn crate_without_artifact_is_skipped_not_failed() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        // No archive artifact in this shard at all.
        let findings = KrewSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent artifact");
        assert!(
            findings.is_empty(),
            "a crate with no archive in this shard must be skipped, got: {findings:?}"
        );
    }

    /// The check must BITE: krew's `ValidatePlugin` runs `spec.version` through
    /// `semver.Parse`, which both (a) rejects a non-semver token outright and
    /// (b) hard-requires the leading `v` (a bare `1.2.3` is rejected). The
    /// schema rejects each with a finding naming the offending field.
    #[test]
    fn non_semver_version_is_reported() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "kubectl-widget");

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");

        // (a) A non-semver token is rejected.
        let mut garbage = yaml_to_json(&manifest).expect("manifest is YAML");
        garbage
            .pointer_mut("/spec")
            .and_then(|v| v.as_object_mut())
            .expect("spec is a map")
            .insert(
                "version".to_string(),
                serde_json::Value::String("not-a-version".to_string()),
            );
        let findings = validate_json("krew", &garbage, KREW_SCHEMA).expect("validation runs");
        let version_finding = findings
            .iter()
            .find(|f| f.field.contains("version"))
            .unwrap_or_else(|| panic!("a finding for the non-semver version; got: {findings:?}"));
        assert_eq!(version_finding.publisher, "krew");

        // (b) A valid-shape semver WITHOUT the required `v` prefix is rejected —
        // krew's semver.Parse hard-rejects it, so the schema must too. This
        // assertion fails if the version pattern admits an optional `v`.
        let mut no_v = yaml_to_json(&manifest).expect("manifest is YAML");
        no_v.pointer_mut("/spec")
            .and_then(|v| v.as_object_mut())
            .expect("spec is a map")
            .insert(
                "version".to_string(),
                serde_json::Value::String("1.2.3".to_string()),
            );
        let no_v_findings = validate_json("krew", &no_v, KREW_SCHEMA).expect("validation runs");
        assert!(
            no_v_findings.iter().any(|f| f.field.contains("version")),
            "a no-`v` version (1.2.3) must be rejected — the pattern requires a \
             leading `v`; got: {no_v_findings:?}"
        );
    }

    /// A required-key omission also bites: dropping the platform `selector`
    /// (krew's `validateSelector` rejects a nil selector) is reported.
    #[test]
    fn missing_platform_selector_is_reported() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "kubectl-widget");

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value = yaml_to_json(&manifest).expect("manifest is YAML");
        value
            .pointer_mut("/spec/platforms/0")
            .and_then(|v| v.as_object_mut())
            .expect("platform is a map")
            .remove("selector");

        let findings = validate_json("krew", &value, KREW_SCHEMA).expect("validation runs");
        assert!(
            findings.iter().any(|f| f.expected.contains("selector")),
            "dropping the required platform selector must be reported, got: {findings:?}"
        );
    }

    /// krew's `validateSelector` tolerates a non-nil but empty `matchLabels`
    /// map (it only errors when both `matchLabels` is nil AND `matchExpressions`
    /// is empty), so the schema must NOT reject `matchLabels: {}`. Guards
    /// against re-tightening `matchLabels` with a `minProperties` that krew
    /// itself does not impose.
    #[test]
    fn empty_match_labels_is_accepted() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "kubectl-widget");

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value = yaml_to_json(&manifest).expect("manifest is YAML");
        // Replace the populated os/arch matchLabels with an empty map.
        value
            .pointer_mut("/spec/platforms/0/selector")
            .and_then(|v| v.as_object_mut())
            .expect("selector is a map")
            .insert(
                "matchLabels".to_string(),
                serde_json::Value::Object(serde_json::Map::new()),
            );

        let findings = validate_json("krew", &value, KREW_SCHEMA).expect("validation runs");
        assert!(
            findings.is_empty(),
            "an empty matchLabels map must conform (krew tolerates it), got: {findings:?}"
        );
    }

    /// A malformed `sha256` (not 64 hex chars) bites: krew's `isValidSHA256`
    /// rejects it with the `^[a-f0-9]{64}$` pattern.
    #[test]
    fn malformed_sha256_is_reported() {
        let cfg = every_option_krew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "kubectl-widget");

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value = yaml_to_json(&manifest).expect("manifest is YAML");
        value
            .pointer_mut("/spec/platforms/0")
            .and_then(|v| v.as_object_mut())
            .expect("platform is a map")
            .insert(
                "sha256".to_string(),
                serde_json::Value::String("NOTHEX".to_string()),
            );

        let findings = validate_json("krew", &value, KREW_SCHEMA).expect("validation runs");
        let sha_finding = findings
            .iter()
            .find(|f| f.field.contains("sha256"))
            .unwrap_or_else(|| panic!("a finding for the malformed sha256; got: {findings:?}"));
        assert_eq!(sha_finding.publisher, "krew");
    }

    /// The `ids` allow-list collector and the one-binary-per-archive collector
    /// must honor identical artifact eligibility. With an `ids` allow-list that
    /// admits one archive and excludes another, the excluded archive's platform
    /// entry (and its binary) must NOT leak into the manifest. Guards the
    /// "secondary collector with a looser predicate" drift the shared
    /// `filter_by_ids` exists to prevent.
    #[test]
    fn ids_excluded_artifact_leaks_no_platform_entry() {
        let cfg = KrewConfig {
            // Only the `main` artifact is eligible; `extra` is filtered out.
            ids: Some(vec!["main".to_string()]),
            ..every_option_krew_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![krew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // The admitted archive (id=main, binary=kubectl-widget) and an excluded
        // one (id=extra, binary=sneaky). Both are Linux archives on the same
        // arch, so a looser walk would collect `sneaky` even though the `ids`
        // filter drops its platform entry.
        add_archive_with_id(&mut ctx, "widget", Some("main"), "kubectl-widget");
        add_archive_with_id(&mut ctx, "widget", Some("extra"), "sneaky");

        let findings = KrewSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "the admitted-only manifest must conform, got: {findings:?}"
        );

        let manifest = render_krew_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let value = yaml_to_json(&manifest).expect("manifest is YAML");
        // Only the admitted archive contributes a platform entry — the
        // excluded one is dropped, so exactly one platform remains.
        let platforms = value
            .pointer("/spec/platforms")
            .and_then(|v| v.as_array())
            .expect("platforms array");
        assert_eq!(
            platforms.len(),
            1,
            "the ids-excluded archive must contribute no platform entry, got: {manifest}"
        );
        // The excluded artifact's binary must not leak into any `bin:` field.
        assert!(
            !manifest.contains("bin: sneaky"),
            "the ids-excluded artifact's binary must not leak into `bin`, got: {manifest}"
        );
        // The admitted artifact's binary IS present.
        assert!(
            manifest.contains("bin: kubectl-widget"),
            "the admitted artifact's binary must be present, got: {manifest}"
        );
    }
}
