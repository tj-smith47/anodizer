//! Scoop manifest schema validation.
//!
//! Scoop's bucket tooling validates every app manifest against the project's
//! published JSON Schema (`ScoopInstaller/Scoop/schema.json`) — a draft-07
//! schema that pins the `architecture` keys to `64bit`/`32bit`/`arm64`, the
//! `hash` format, and the `version`/`homepage`/`license` required set. anodizer
//! renders that manifest per crate; this validator renders the exact JSON the
//! live publish would push — via the same render path — and checks it against
//! the vendored schema, so a structural defect (an out-of-place architecture
//! key, a wrong-typed `bin`, a missing required field) surfaces in the
//! emission-validate pass rather than after a bucket commit has shipped.

use anodizer_core::context::Context;
use anyhow::Result;

use super::{
    PublisherSchemaValidator, SchemaFinding, TagResolver, validate_json, with_validated_crate_scope,
};
use crate::scoop::{
    crate_has_scoop_artifacts, is_scoop_per_crate_configured, reject_unsupported_use,
    render_scoop_manifest_for_crate,
};

/// The Scoop project's vendored app-manifest schema (draft-07). Pinned and
/// embedded so validation is fully offline; refresh via `schemas/SOURCES.md`.
const SCOOP_SCHEMA: &str = include_str!("../../schemas/scoop.schema.json");

/// Validates anodizer's rendered Scoop manifests against the Scoop project's
/// published JSON Schema.
pub(crate) struct ScoopSchemaValidator;

impl PublisherSchemaValidator for ScoopSchemaValidator {
    fn publisher(&self) -> &'static str {
        "scoop"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk exactly the crate set the live scoop publisher's `run` iterates
        // (honoring `--crate` selection, else every scoop-configured crate) so
        // the validated set equals the published set in all config modes.
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_scoop_per_crate_configured);
        for crate_name in &selected {
            if !is_scoop_per_crate_configured(ctx, crate_name) {
                continue;
            }
            // A real release always produces a Windows archive artifact, but a
            // target-restricted determinism shard may build none for this crate
            // (e.g. a Linux/macOS-only shard). The self-skip is gated on the
            // partial-shard signal exactly as homebrew/nix gate theirs: on a FULL
            // build, an empty archive set is a genuine misconfiguration (scoop
            // configured but nothing it can package), so it must fall through to
            // the render and ERROR — the same "no Windows archive artifact" bail
            // the live publish path hits — rather than silently skip. The probe
            // is the SAME collector the live publish uses, so the validated set
            // never diverges.
            let Some(scoop_cfg) = ctx
                .config
                .find_crate(crate_name)
                .and_then(|c| c.publish.as_ref())
                .and_then(|p| p.scoop.clone())
            else {
                continue;
            };
            // Reject an installer `use:` here, BEFORE the artifact-presence skip
            // below: a single-target / sharded snapshot may produce no matching
            // artifact, in which case the crate would be skipped and the publish-
            // time `reject_unsupported_use` in the render path would never run —
            // letting an unshippable `use: msi/nsis/wix/exe` config slip past
            // `check`. Surfacing it here makes the config error fail validation
            // independent of which artifacts this run built.
            reject_unsupported_use(scoop_cfg.use_artifact.as_deref(), crate_name)?;
            if ctx.is_target_restricted_build()
                && !crate_has_scoop_artifacts(ctx, crate_name, &scoop_cfg)
            {
                log.verbose(&format!(
                    "skipped scoop schema validation for crate '{}' — produced no Windows \
                     archive artifact in this target-restricted shard",
                    crate_name
                ));
                ctx.emission_skips.remember(
                    crate::snapshot_validation::EMISSION_SKIP_STAGE,
                    &format!("{crate_name} scoop"),
                    "no Windows archive artifact in this target-restricted shard",
                );
                continue;
            }

            // Render + validate under THIS crate's own version (workspace
            // per-crate independent-version mode renders each crate's manifest
            // against its own version, not the first crate's).
            let crate_findings = with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                // `None` means the publisher would skip this crate
                // (skip_upload / falsy `if`) — nothing to validate.
                let Some(manifest) = render_scoop_manifest_for_crate(ctx, crate_name, &log)? else {
                    return Ok(Vec::new());
                };
                let value: serde_json::Value = serde_json::from_str(&manifest).map_err(|e| {
                    anyhow::anyhow!("scoop: rendered manifest is not valid JSON: {e}")
                })?;
                validate_json("scoop", &value, SCOOP_SCHEMA)
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
        CrateConfig, PublishConfig, RepositoryConfig, ScmRepoConfig, ScoopConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;
    use crate::scoop::render_scoop_manifest_for_crate;

    /// A `ScoopConfig` exercising every manifest-affecting option, with values
    /// the scoop schema accepts (valid SPDX license, URL homepage, arrays).
    fn every_option_scoop_cfg() -> ScoopConfig {
        ScoopConfig {
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("scoop-bucket".to_string()),
                ..Default::default()
            }),
            commit_author: None,
            name: Some("widget".to_string()),
            directory: Some("bucket".to_string()),
            description: Some("A widget management tool".to_string()),
            license: Some("MIT".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            persist: Some(vec!["data".to_string(), "config".to_string()]),
            depends: Some(vec!["main/7zip".to_string()]),
            pre_install: Some(vec!["echo pre-install".to_string()]),
            post_install: Some(vec!["echo post-install".to_string()]),
            shortcuts: Some(vec![vec!["widget.exe".to_string(), "Widget".to_string()]]),
            checkver: Some("github".to_string()),
            skip_upload: None,
            commit_msg_template: Some("Scoop: {{ ProjectName }} {{ Tag }}".to_string()),
            ids: None,
            url_template: None,
            use_artifact: None,
            amd64_variant: Some(anodizer_core::config::Amd64Variant::V1),
            required: Some(true),
            if_condition: None,
            retain_on_rollback: None,
        }
    }

    fn scoop_crate(crate_name: &str, tag_template: &str, cfg: ScoopConfig) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: Some(tag_template.to_string()),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                    token: None,
                }),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                scoop: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Add a Windows zip archive (with the sha256 + url + format metadata the
    /// manifest's `architecture` block needs) plus the Windows Binary artifact
    /// that drives the `bin` entries.
    fn add_windows_zip(ctx: &mut Context, crate_name: &str, binary: &str) {
        let target = "x86_64-pc-windows-msvc";
        let mut archive_meta = HashMap::new();
        archive_meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{crate_name}-{target}.zip"
            ),
        );
        archive_meta.insert("sha256".to_string(), "a".repeat(64));
        archive_meta.insert("format".to_string(), "zip".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.zip")),
            name: format!("{crate_name}-{target}.zip"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: archive_meta,
            size: None,
        });

        let mut bin_meta = HashMap::new();
        bin_meta.insert("binary".to_string(), binary.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: std::path::PathBuf::from(format!("/dist/{binary}.exe")),
            name: format!("{binary}.exe"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: bin_meta,
            size: None,
        });
    }

    /// Add one Windows zip Archive carrying explicit `id` and `binary` metadata
    /// — the shape the `ids` filter and the `bin`-name collector key off. Used
    /// to prove the two collectors honor identical eligibility: an `ids`-excluded
    /// archive must contribute neither an `architecture` entry nor a `bin` name.
    fn add_windows_zip_with_id(ctx: &mut Context, crate_name: &str, id: &str, binary: &str) {
        let target = "x86_64-pc-windows-msvc";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{target}.zip"
            ),
        );
        meta.insert("sha256".to_string(), "b".repeat(64));
        meta.insert("format".to_string(), "zip".to_string());
        meta.insert("id".to_string(), id.to_string());
        meta.insert("binary".to_string(), binary.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{binary}-{target}.zip")),
            name: format!("{binary}-{target}.zip"),
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
    /// the schema-expected field.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let findings = ScoopSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate manifest must conform, got: {findings:?}"
        );

        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let value: serde_json::Value =
            serde_json::from_str(&manifest).expect("rendered manifest is JSON");
        let obj = value.as_object().expect("manifest is an object");

        assert_eq!(obj.get("version").and_then(|v| v.as_str()), Some("1.0.0"));
        assert_eq!(
            obj.get("homepage").and_then(|v| v.as_str()),
            Some("https://acme.example/widget")
        );
        assert_eq!(obj.get("license").and_then(|v| v.as_str()), Some("MIT"));
        // The Windows zip lands as `architecture.64bit.url` with its sha256.
        let url64 = value
            .pointer("/architecture/64bit/url")
            .and_then(|v| v.as_str());
        assert_eq!(
            url64,
            Some(
                "https://github.com/acme/widget/releases/download/v1.0.0/widget-x86_64-pc-windows-msvc.zip"
            )
        );
        assert!(
            value
                .pointer("/architecture/64bit/hash")
                .and_then(|v| v.as_str())
                .is_some_and(|h| h.len() == 64),
            "hash lands under architecture.64bit"
        );
        // `bin` is an array carrying the binary name from artifact metadata.
        let bin = value
            .pointer("/architecture/64bit/bin")
            .and_then(|v| v.as_array())
            .expect("bin array");
        assert!(bin.iter().any(|b| b.as_str() == Some("widget.exe")));
        // The optional array fields land at the document root.
        assert_eq!(
            obj.get("persist").and_then(|v| v.as_array()).map(Vec::len),
            Some(2)
        );
        assert!(obj.contains_key("depends"));
        assert!(obj.contains_key("pre_install"));
        assert!(obj.contains_key("post_install"));
        assert!(obj.contains_key("shortcuts"));
    }

    /// (b) Workspace-lockstep mode: multiple crates share one version/tag. Each
    /// crate's manifest must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = scoop_crate(
            "alpha",
            "v{{ .Version }}",
            ScoopConfig {
                name: Some("alpha".to_string()),
                ..every_option_scoop_cfg()
            },
        );
        let beta = scoop_crate(
            "beta",
            "v{{ .Version }}",
            ScoopConfig {
                name: Some("beta".to_string()),
                ..every_option_scoop_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        // Lockstep: a single global version names every crate's archives.
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "alpha", "alpha");
        add_windows_zip(&mut ctx, "beta", "beta");

        let findings = ScoopSchemaValidator
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
        let alpha = scoop_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            ScoopConfig {
                name: Some("alpha".to_string()),
                ..every_option_scoop_cfg()
            },
        );
        let beta = scoop_crate(
            "beta",
            "beta-v{{ .Version }}",
            ScoopConfig {
                name: Some("beta".to_string()),
                ..every_option_scoop_cfg()
            },
        );

        // alpha @ 2.0.0
        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_windows_zip(&mut ctx_a, "alpha", "alpha");
        let findings_a = ScoopSchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let manifest_a = render_scoop_manifest_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest_a.contains("\"version\": \"2.0.0\""),
            "alpha manifest stamps its own version, got: {manifest_a}"
        );

        // beta @ 3.1.0 — its own version stamps its own manifest.
        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_windows_zip(&mut ctx_b, "beta", "beta");
        let findings_b = ScoopSchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let manifest_b = render_scoop_manifest_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            manifest_b.contains("\"version\": \"3.1.0\""),
            "beta manifest stamps its own version, got: {manifest_b}"
        );
    }

    /// A TARGET-RESTRICTED determinism shard that built no Windows archive for a
    /// scoop-configured crate must SKIP it (zero findings, no error) rather than
    /// trip the publisher's "no Windows archive" guard — the archive
    /// legitimately landed on another shard. The self-skip is gated on
    /// `partial_target`, so this holds only on a shard.
    #[test]
    fn partial_shard_without_windows_artifact_is_skipped_not_failed() {
        use anodizer_core::partial::PartialTarget;
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        ctx.options.partial_target = Some(PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
        ]));
        // Only a linux archive — no Windows archive in this shard.
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            "https://github.com/acme/widget/releases/download/v1.0.0/widget-linux.tar.gz"
                .to_string(),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-x86_64-unknown-linux-gnu.tar.gz"),
            name: "widget-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let findings = ScoopSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent Windows artifact");
        assert!(
            findings.is_empty(),
            "a crate with no Windows archive in this shard must be skipped, got: {findings:?}"
        );
    }

    /// On a FULL build (no `partial_target`) a scoop-configured crate that
    /// produced NO Windows archive is a genuine misconfiguration — the same "no
    /// Windows archive artifact" bail the live publish hits must surface at
    /// `check`/`--snapshot` rather than being silently skipped (the
    /// failure-hiding class this closes).
    #[test]
    fn full_build_without_windows_artifact_errors() {
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // Only a linux archive — no Windows archive; partial_target None.
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            "https://github.com/acme/widget/releases/download/v1.0.0/widget-linux.tar.gz"
                .to_string(),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-x86_64-unknown-linux-gnu.tar.gz"),
            name: "widget-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let err = ScoopSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect_err("a full build with scoop configured but no Windows archive must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no Windows archive artifact") && msg.contains("scoop"),
            "surfaces the genuine full-build absence, naming scoop: {msg}"
        );
    }

    /// A PRESENT Windows archive missing its sha256 is a REAL defect the live
    /// publish refuses to push — the validator must SURFACE it (the render's
    /// bail propagates through `validate`), NOT collapse it to a shard skip.
    /// The presence probe is a bare `bool` that does not read sha256, so this
    /// is the validator-PATH guard against a future refactor reintroducing the
    /// silent-skip bug (propagation itself is proven one layer down in
    /// `scoop_sha256_empty_metadata_bails_with_actionable_error`).
    #[test]
    fn present_archive_missing_sha256_surfaces_not_skips() {
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // A Windows zip that is PRESENT (clears the presence probe) but carries
        // no sha256 — the manifest render bails on it.
        let target = "x86_64-pc-windows-msvc";
        let mut archive_meta = HashMap::new();
        archive_meta.insert(
            "url".to_string(),
            format!("https://acme.example/v1.0.0/widget-{target}.zip"),
        );
        archive_meta.insert("format".to_string(), "zip".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/widget-{target}.zip")),
            name: format!("widget-{target}.zip"),
            target: Some(target.to_string()),
            crate_name: "widget".to_string(),
            metadata: archive_meta,
            size: None,
        });
        let mut bin_meta = HashMap::new();
        bin_meta.insert("binary".to_string(), "widget".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: std::path::PathBuf::from("/dist/widget.exe"),
            name: "widget.exe".to_string(),
            target: Some(target.to_string()),
            crate_name: "widget".to_string(),
            metadata: bin_meta,
            size: None,
        });

        let err = ScoopSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect_err(
                "a present-but-broken Windows archive (missing sha256) must surface as an \
             error, not a silent skip",
            );
        assert!(
            format!("{err:#}").contains("sha256"),
            "the surfaced error must name the missing sha256, got: {err:#}"
        );
    }

    /// The check must BITE: the `architecture` block pins its keys to
    /// `64bit`/`32bit`/`arm64` with `additionalProperties: false`, so an
    /// out-of-place architecture key is rejected — with a finding naming the
    /// offending field.
    #[test]
    fn invalid_architecture_key_is_reported() {
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest is JSON");
        // Re-key the valid `64bit` entry to a value outside the schema's
        // architecture key set, leaving the document otherwise well-formed.
        let arch = value
            .pointer_mut("/architecture")
            .and_then(|v| v.as_object_mut())
            .expect("architecture is a map");
        let entry = arch.remove("64bit").expect("64bit entry present");
        arch.insert("sparc".to_string(), entry);

        let findings = validate_json("scoop", &value, SCOOP_SCHEMA).expect("validation runs");
        let arch_finding = findings
            .iter()
            .find(|f| f.field.contains("architecture"))
            .unwrap_or_else(|| {
                panic!("a finding for the out-of-place architecture key; got: {findings:?}")
            });
        assert_eq!(arch_finding.publisher, "scoop");
    }

    /// A required-key omission also bites: dropping `version` from the manifest
    /// is rejected at the document root.
    #[test]
    fn missing_required_key_is_reported() {
        let cfg = every_option_scoop_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest is JSON");
        value
            .as_object_mut()
            .expect("manifest is a map")
            .remove("version");

        let findings = validate_json("scoop", &value, SCOOP_SCHEMA).expect("validation runs");
        assert!(
            findings.iter().any(|f| f.expected.contains("version")),
            "dropping a required key must be reported, got: {findings:?}"
        );
    }

    /// The `bin`-name collector and the `architecture`-entry collector must
    /// honor identical artifact eligibility. With an `ids` allow-list that
    /// admits one archive and excludes another, the excluded archive's binary
    /// must NOT leak into the manifest's `bin` field — and its `architecture`
    /// entry must be absent too. Guards the "secondary collector with a looser
    /// predicate" drift the shared `filters.matches` exists to prevent.
    #[test]
    fn ids_excluded_artifact_leaks_neither_arch_entry_nor_bin_name() {
        let cfg = ScoopConfig {
            // Only the `main` artifact is eligible; `extra` is filtered out.
            ids: Some(vec!["main".to_string()]),
            ..every_option_scoop_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // The admitted archive (id=main, binary=widget) and an excluded one
        // (id=extra, binary=sneaky). Both are Windows zips on the same arch, so
        // a looser Windows-only `bin` walk would collect `sneaky.exe` even
        // though the `ids` filter drops its arch entry.
        add_windows_zip_with_id(&mut ctx, "widget", "main", "widget");
        add_windows_zip_with_id(&mut ctx, "widget", "extra", "sneaky");

        let findings = ScoopSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "the admitted-only manifest must conform, got: {findings:?}"
        );

        let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let value: serde_json::Value = serde_json::from_str(&manifest).expect("manifest is JSON");

        // The excluded artifact's URL must not appear anywhere (its arch entry
        // is absent).
        assert!(
            !manifest.contains("sneaky-x86_64-pc-windows-msvc.zip"),
            "the ids-excluded archive must contribute no architecture entry, got: {manifest}"
        );
        // Its binary must not leak into the `bin` field.
        let bin = value
            .pointer("/architecture/64bit/bin")
            .and_then(|v| v.as_array())
            .expect("bin array");
        let bin_strs: Vec<&str> = bin.iter().filter_map(|b| b.as_str()).collect();
        assert!(
            !bin_strs.contains(&"sneaky.exe"),
            "the ids-excluded artifact's binary must not leak into `bin`, got: {bin_strs:?}"
        );
        // The admitted artifact's binary IS present.
        assert!(
            bin_strs.contains(&"widget.exe"),
            "the admitted artifact's binary must be present, got: {bin_strs:?}"
        );
    }

    /// `check`-level validation must reject an unshippable `use: msi` scoop
    /// config EVEN WHEN this run produced no matching artifact. A linux-only /
    /// sharded snapshot builds no MSI, so `crate_has_scoop_artifacts` would
    /// otherwise short-circuit the validator before the render path's
    /// `reject_unsupported_use` ever ran — letting the bad config reach a bucket
    /// commit. The reject now fires from `validate` ahead of that skip-gate.
    /// Covers all three config modes (single / lockstep / per-crate) since the
    /// reject is keyed only on the per-crate `use:` value.
    #[test]
    fn check_rejects_use_msi_for_scoop_across_config_modes() {
        let msi_cfg = || ScoopConfig {
            use_artifact: Some("msi".to_string()),
            ..every_option_scoop_cfg()
        };

        let assert_rejects = |ctx: &mut Context, mode: &str| {
            let err = ScoopSchemaValidator
                .validate(
                    ctx,
                    &crate::schema_validation::test_current_version_resolver(),
                )
                .expect_err(&format!(
                    "{mode}: check-time validation must reject `use: msi` for scoop"
                ));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("use: msi") && msg.contains("scoop"),
                "{mode}: the rejection must name the unsupported scoop `use`, got: {msg}"
            );
        };

        // (a) Single-crate, with NO artifact of any kind produced — the case the
        // old skip-gate swallowed.
        let mut ctx_single = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![scoop_crate("widget", "v{{ .Version }}", msi_cfg())])
            .build();
        scope_version(&mut ctx_single, "1.0.0");
        assert_rejects(&mut ctx_single, "single-crate");

        // (b) Workspace-lockstep: one bad crate among several must still bite.
        let mut ctx_lockstep = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![
                scoop_crate(
                    "alpha",
                    "v{{ .Version }}",
                    ScoopConfig {
                        name: Some("alpha".to_string()),
                        ..every_option_scoop_cfg()
                    },
                ),
                scoop_crate(
                    "beta",
                    "v{{ .Version }}",
                    ScoopConfig {
                        name: Some("beta".to_string()),
                        ..msi_cfg()
                    },
                ),
            ])
            .build();
        scope_version(&mut ctx_lockstep, "1.0.0");
        add_windows_zip(&mut ctx_lockstep, "alpha", "alpha");
        assert_rejects(&mut ctx_lockstep, "workspace-lockstep");

        // (c) Workspace per-crate: the offending crate is the `--crate`-selected
        // one and renders under its own version/tag.
        let mut ctx_per_crate = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![
                scoop_crate(
                    "alpha",
                    "alpha-v{{ .Version }}",
                    ScoopConfig {
                        name: Some("alpha".to_string()),
                        ..every_option_scoop_cfg()
                    },
                ),
                scoop_crate(
                    "beta",
                    "beta-v{{ .Version }}",
                    ScoopConfig {
                        name: Some("beta".to_string()),
                        ..msi_cfg()
                    },
                ),
            ])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_per_crate, "3.1.0");
        assert_rejects(&mut ctx_per_crate, "workspace-per-crate");
    }
}
