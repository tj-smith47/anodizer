//! WinGet manifest schema validation.
//!
//! WinGet's `microsoft/winget-pkgs` submission pipeline validates every
//! manifest against Microsoft's published JSON Schemas before a maintainer ever
//! sees the PR. anodizer emits the three-file `version` / `installer` /
//! `defaultLocale` manifest set (ManifestVersion 1.12.0); this validator renders
//! that exact set for every in-scope crate — via the same param-assembly path
//! the live publish uses — and checks each document against the matching
//! vendored schema, so a structural defect (a wrong-typed value, an
//! out-of-enum `Architecture`, a missing required key) is caught in the
//! emission-validate pass rather than after a real release has opened a PR.

use anodizer_core::context::Context;
use anyhow::Result;

use super::{
    PublisherSchemaValidator, SchemaFinding, TagResolver, validate_json,
    with_validated_crate_scope, yaml_to_json,
};
use crate::winget::{
    crate_has_winget_installer_artifacts, is_winget_per_crate_configured,
    render_winget_manifests_for_crate,
};

/// Microsoft's vendored manifest schemas (ManifestVersion 1.12.0). Pinned and
/// embedded so validation is fully offline; refresh via `schemas/SOURCES.md`.
/// The emitted `ManifestVersion` (`crate::winget`) and these schema versions
/// must agree — a renderer bump requires a matching schema re-vendor.
const VERSION_SCHEMA: &str = include_str!("../../schemas/winget.version.1.12.0.schema.json");
const INSTALLER_SCHEMA: &str = include_str!("../../schemas/winget.installer.1.12.0.schema.json");
const DEFAULT_LOCALE_SCHEMA: &str =
    include_str!("../../schemas/winget.defaultLocale.1.12.0.schema.json");

/// Validates anodizer's rendered WinGet manifests against Microsoft's
/// published JSON Schemas.
pub(crate) struct WingetSchemaValidator;

impl PublisherSchemaValidator for WingetSchemaValidator {
    fn publisher(&self) -> &'static str {
        "winget"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk exactly the crate set the live winget publisher's `run` iterates
        // (honoring `--crate` selection, else every winget-configured crate) so
        // the validated set equals the published set in all config modes.
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_winget_per_crate_configured);
        for crate_name in &selected {
            if !is_winget_per_crate_configured(ctx, crate_name) {
                continue;
            }
            // A real release always produces a Windows installer artifact, but a
            // target-restricted determinism shard may build none for this crate
            // (e.g. a Linux/macOS-only shard). The self-skip is gated on the
            // partial-shard signal exactly as homebrew/nix gate theirs: on a FULL
            // build, an empty installer set is a genuine misconfiguration (winget
            // configured but nothing it can package), so it must fall through to
            // the render and ERROR — the same "no Windows archive or binary
            // artifact" bail the live publish path hits (`collect_winget_installers`)
            // — rather than silently skip. The probe is the SAME collector the
            // live publish uses, so the validated set never diverges.
            let Some(winget_cfg) = ctx
                .config
                .find_crate(crate_name)
                .and_then(|c| c.publish.as_ref())
                .and_then(|p| p.winget.clone())
            else {
                continue;
            };
            if ctx.is_target_restricted_build()
                && !crate_has_winget_installer_artifacts(ctx, crate_name, &winget_cfg)
            {
                log.verbose(&format!(
                    "skipped winget schema validation for crate '{}' — produced no Windows \
                     installer artifact in this target-restricted shard",
                    crate_name
                ));
                ctx.emission_skips.remember(
                    crate::snapshot_validation::EMISSION_SKIP_STAGE,
                    &format!("{crate_name} winget"),
                    "no Windows installer artifact in this target-restricted shard",
                );
                continue;
            }

            // Render + validate under THIS crate's own version so the manifest's
            // `PackageVersion` is the version a real release would stamp, not the
            // first crate's (workspace per-crate independent-version mode).
            let crate_findings = with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                let mut out = Vec::new();
                // `None` means the publisher would skip this crate
                // (skip_upload / falsy `if`) — nothing to validate.
                if let Some(rendered) = render_winget_manifests_for_crate(ctx, crate_name, &log)? {
                    out.extend(validate_manifest(&rendered.version_yaml, VERSION_SCHEMA)?);
                    out.extend(validate_manifest(
                        &rendered.installer_yaml,
                        INSTALLER_SCHEMA,
                    )?);
                    out.extend(validate_manifest(
                        &rendered.locale_yaml,
                        DEFAULT_LOCALE_SCHEMA,
                    )?);
                }
                Ok(out)
            })?;
            findings.extend(crate_findings);
        }

        Ok(findings)
    }
}

/// Convert one rendered YAML manifest to JSON and validate it against `schema`,
/// returning a [`SchemaFinding`] per violation.
fn validate_manifest(yaml: &str, schema: &str) -> Result<Vec<SchemaFinding>> {
    let value = yaml_to_json(yaml)?;
    validate_json("winget", &value, schema)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        CrateConfig, PublishConfig, RepositoryConfig, WingetConfig, WingetDependency,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;
    use crate::winget::render_winget_manifests_for_crate;

    /// A `WingetConfig` exercising every operator-exposed option, with values
    /// that satisfy the registry schemas' length / pattern / URL constraints.
    fn every_option_winget_cfg() -> WingetConfig {
        WingetConfig {
            name: Some("Widget".to_string()),
            package_name: Some("Widget Tool".to_string()),
            package_identifier: Some("AcmeCo.Widget".to_string()),
            publisher: Some("Acme Co".to_string()),
            publisher_url: Some("https://acme.example".to_string()),
            publisher_support_url: Some("https://acme.example/support".to_string()),
            privacy_url: Some("https://acme.example/privacy".to_string()),
            author: Some("Acme Engineering".to_string()),
            copyright: Some("Copyright (c) 2026 Acme Co".to_string()),
            copyright_url: Some("https://acme.example/copyright".to_string()),
            license: Some("MIT".to_string()),
            license_url: Some("https://acme.example/license".to_string()),
            short_description: Some("A widget management tool".to_string()),
            description: Some("A full-featured tool for managing widgets.".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            url_template: Some(
                "https://github.com/acme/widget/releases/download/v{{ .Version }}/widget-{{ .Arch }}.zip"
                    .to_string(),
            ),
            ids: None,
            skip_upload: None,
            commit_msg_template: Some("New version: {{ PackageIdentifier }} {{ Version }}".to_string()),
            path: None,
            release_notes: Some("Initial widget release.".to_string()),
            release_notes_url: Some("https://acme.example/widget/notes".to_string()),
            installation_notes: Some("Add widget.exe to PATH after install.".to_string()),
            tags: Some(vec!["widget".to_string(), "cli".to_string()]),
            dependencies: Some(vec![WingetDependency {
                package_identifier: "Acme.Runtime".to_string(),
                minimum_version: Some("1.0.0".to_string()),
                ..Default::default()
            }]),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("winget-pkgs-fork".to_string()),
                ..Default::default()
            }),
            commit_author: None,
            product_code: Some("{ACME-WIDGET-0001}".to_string()),
            moniker: Some("widget".to_string()),
            documentations: Some(vec![anodizer_core::config::WingetDocumentation {
                label: "User Guide".to_string(),
                url: "https://acme.example/widget/guide".to_string(),
            }]),
            upgrade_behavior: Some("install".to_string()),
            silent_switch: None,
            use_artifact: None,
            amd64_variant: Some(anodizer_core::config::Amd64Variant::V1),
            post_publish_poll: None,
            update_existing_pr: None,
            required: Some(true),
            if_condition: None,
            retain_on_rollback: None,
        }
    }

    fn winget_crate(crate_name: &str, tag_template: &str, cfg: WingetConfig) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            publish: Some(PublishConfig {
                winget: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Add a windows zip archive artifact (with the sha256 + url metadata the
    /// installer manifest needs) plus the windows Binary artifact that drives
    /// the nested-installer-file entries.
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

    /// Re-scope the global template vars to the version a release would stamp
    /// for `version` — the same shape `with_crate_scope` applies before the
    /// publish stage invokes a per-crate publisher.
    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    /// (a) Single-crate mode: one crate, one tag. Every exposed option set, and
    /// the rendered manifests must conform to all three schemas with zero
    /// findings — and land their values in the schema-expected fields.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let findings = WingetSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate manifests must conform, got: {findings:?}"
        );

        let rendered = render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        // Installer manifest: the windows zip lands as an Installers entry with
        // the schema-expected Architecture / InstallerUrl.
        assert!(rendered.installer_yaml.contains("Installers:"));
        assert!(rendered.installer_yaml.contains("Architecture: x64"));
        assert!(rendered.installer_yaml.contains("InstallerUrl:"));
        assert!(rendered.installer_yaml.contains("InstallerSha256:"));
        // Locale manifest: the descriptive options land in their schema fields.
        assert!(rendered.locale_yaml.contains("PackageName: Widget Tool"));
        assert!(rendered.locale_yaml.contains("Publisher: Acme Co"));
        assert!(rendered.locale_yaml.contains("License: MIT"));
        assert!(
            rendered
                .locale_yaml
                .contains("ShortDescription: A widget management tool")
        );
        // Version manifest: the identifier + version land on the version doc.
        assert!(
            rendered
                .version_yaml
                .contains("PackageIdentifier: AcmeCo.Widget")
        );
        assert!(rendered.version_yaml.contains("PackageVersion: 1.0.0"));
    }

    /// (b) Workspace-lockstep mode: multiple crates share one version/tag. Each
    /// crate's manifest set must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = winget_crate(
            "alpha",
            "v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Alpha".to_string()),
                ..every_option_winget_cfg()
            },
        );
        let beta = winget_crate(
            "beta",
            "v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Beta".to_string()),
                ..every_option_winget_cfg()
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

        let findings = WingetSchemaValidator
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

    /// (c) Workspace per-crate mode: each crate carries its own
    /// tag_template/version. The publish stage scopes the global `Version` to
    /// the per-crate value before invoking the publisher, so the validator (run
    /// per-crate via `--crate`) must conform under each crate's own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = winget_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Alpha".to_string()),
                ..every_option_winget_cfg()
            },
        );
        let beta = winget_crate(
            "beta",
            "beta-v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Beta".to_string()),
                ..every_option_winget_cfg()
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
        let findings_a = WingetSchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let rendered_a =
            render_winget_manifests_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
                .expect("render ok")
                .expect("not skipped");
        assert!(rendered_a.version_yaml.contains("PackageVersion: 2.0.0"));
        assert!(
            rendered_a
                .version_yaml
                .contains("PackageIdentifier: AcmeCo.Alpha")
        );

        // beta @ 3.1.0 — its own version stamps its own manifests.
        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_windows_zip(&mut ctx_b, "beta", "beta");
        let findings_b = WingetSchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let rendered_b =
            render_winget_manifests_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
                .expect("render ok")
                .expect("not skipped");
        assert!(rendered_b.version_yaml.contains("PackageVersion: 3.1.0"));
        assert!(
            rendered_b
                .version_yaml
                .contains("PackageIdentifier: AcmeCo.Beta")
        );
    }

    /// (d) Workspace per-crate INDEPENDENT-version mode, multi-crate in ONE
    /// context (the live publish-stage shape, NOT a `--crate`-narrowed run).
    ///
    /// alpha and beta carry DIFFERENT versions. The global `Version` is the
    /// FIRST crate's (2.0.0) — exactly what `populate_git_vars` derives. Before
    /// the fix, the validator rendered EVERY crate's `PackageVersion` against
    /// that global 2.0.0, so beta's manifest carried the wrong version. With the
    /// per-crate resolver each crate is re-scoped to its own tag, so beta renders
    /// 3.1.0. This pins each crate's rendered version to ITS OWN version and
    /// fails against the pre-fix global-version code.
    #[test]
    fn multi_crate_independent_versions_render_each_crate_own_version() {
        let alpha = winget_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Alpha".to_string()),
                ..every_option_winget_cfg()
            },
        );
        let beta = winget_crate(
            "beta",
            "beta-v{{ .Version }}",
            WingetConfig {
                package_identifier: Some("AcmeCo.Beta".to_string()),
                ..every_option_winget_cfg()
            },
        );

        // One ctx, BOTH crates, NO `--crate` narrowing. Global Version is the
        // first crate's (2.0.0) — the bug's poison value for beta.
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "2.0.0");
        add_windows_zip(&mut ctx, "alpha", "alpha");
        add_windows_zip(&mut ctx, "beta", "beta");

        // Per-crate resolver: each crate maps to its OWN independent version.
        let resolver = |_: &Context, c: &CrateConfig| {
            Some(match c.name.as_str() {
                "beta" => "3.1.0".to_string(),
                _ => "2.0.0".to_string(),
            })
        };

        // The whole driver must pass: each crate's manifest conforms under its
        // own version.
        let findings = WingetSchemaValidator
            .validate(&mut ctx, &resolver)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "independent-version multi-crate manifests must conform, got: {findings:?}"
        );

        // Prove the load-bearing claim: rendering beta UNDER ITS OWN SCOPE stamps
        // beta's version (3.1.0), NOT the global first-crate 2.0.0. Without the
        // per-crate scope this asserts `PackageVersion: 2.0.0` and fails.
        let rendered_beta = crate::schema_validation::with_validated_crate_scope(
            &mut ctx,
            "beta",
            &resolver,
            |ctx| {
                let r = render_winget_manifests_for_crate(ctx, "beta", &ctx.logger("publish"))?
                    .expect("beta not skipped");
                Ok(vec![SchemaFinding {
                    publisher: "winget".to_string(),
                    field: "PackageVersion".to_string(),
                    expected: r.version_yaml,
                }])
            },
        )
        .expect("scoped render ok");
        let beta_yaml = &rendered_beta[0].expected;
        assert!(
            beta_yaml.contains("PackageVersion: 3.1.0"),
            "beta must render its OWN version 3.1.0, not the global first-crate \
             2.0.0; got:\n{beta_yaml}"
        );
        assert!(
            !beta_yaml.contains("PackageVersion: 2.0.0"),
            "beta must NOT carry the first crate's version; got:\n{beta_yaml}"
        );
    }

    /// A TARGET-RESTRICTED determinism shard that built no Windows installer for
    /// a winget-configured crate must SKIP it (zero findings, no error) rather
    /// than trip the publisher's "no Windows artifact" guard — the installer
    /// legitimately landed on another shard. The self-skip is gated on
    /// `partial_target`, so this holds only on a shard.
    #[test]
    fn partial_shard_without_windows_artifact_is_skipped_not_failed() {
        use anodizer_core::partial::PartialTarget;
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        ctx.options.partial_target = Some(PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
        ]));
        // Only a linux archive — no Windows installer in this shard.
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

        let findings = WingetSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent Windows artifact");
        assert!(
            findings.is_empty(),
            "a crate with no Windows installer in this shard must be skipped, got: {findings:?}"
        );
    }

    /// The portable-binary counterpart of the archive shard-skip above: a
    /// linux-only `UploadableBinary` (no Windows target) for a winget-configured
    /// crate must SKIP on a TARGET-RESTRICTED shard — the shard-guard and the
    /// live collector share one Windows predicate, so a non-Windows portable
    /// binary never tricks the guard into driving the renderer past
    /// `collect_winget_installers`' "no Windows artifact" bail.
    #[test]
    fn partial_shard_with_only_linux_portable_binary_is_skipped_not_failed() {
        use anodizer_core::partial::PartialTarget;
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        ctx.options.partial_target = Some(PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
        ]));
        // Only a linux portable binary — no Windows installer in this shard.
        let mut meta = HashMap::new();
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("binary".to_string(), "widget".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::UploadableBinary,
            path: std::path::PathBuf::from("/dist/widget-x86_64-unknown-linux-gnu"),
            name: "widget-x86_64-unknown-linux-gnu".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let findings = WingetSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent Windows artifact");
        assert!(
            findings.is_empty(),
            "a crate with only a linux portable binary must be skipped, got: {findings:?}"
        );
    }

    /// A PRESENT Windows archive missing its sha256 is a REAL defect the live
    /// publish refuses to push — the validator must SURFACE it (the render's
    /// bail propagates through `validate`), NOT collapse it to a shard skip.
    /// The presence probe is a bare `bool` that does not read sha256, so this
    /// is the validator-PATH guard against a future refactor reintroducing the
    /// silent-skip bug (propagation itself is proven one layer down in
    /// `winget_archive_without_sha256_metadata_bails_with_actionable_error`).
    #[test]
    fn present_archive_missing_sha256_surfaces_not_skips() {
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // A Windows zip that is PRESENT (clears the presence probe) but carries
        // no sha256 — the installer render bails on it.
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

        let err = WingetSchemaValidator
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

    /// On a FULL build (no `partial_target`) a winget-configured crate that
    /// produced NO Windows installer is a genuine misconfiguration — the same
    /// `collect_winget_installers` "no Windows archive or binary artifact" bail
    /// the live publish hits must surface at `check`/`--snapshot` rather than
    /// being silently skipped (the failure-hiding class this closes).
    #[test]
    fn full_build_without_windows_artifact_errors() {
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // Only a linux archive — no Windows installer; partial_target None.
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

        let err = WingetSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect_err("a full build with winget configured but no Windows installer must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no Windows archive or binary artifact") && msg.contains("winget"),
            "surfaces the genuine full-build absence, naming winget: {msg}"
        );
    }

    /// The check must BITE: an installer manifest whose `Architecture` is set to
    /// an out-of-enum value is rejected by the installer schema, with a finding
    /// naming the offending field and the schema's expectation.
    #[test]
    fn invalid_architecture_enum_is_reported() {
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let rendered = render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        // Mutate the valid `x64` to a value outside the schema's Architecture
        // enum, leaving the document otherwise well-formed.
        let broken = rendered
            .installer_yaml
            .replace("Architecture: x64", "Architecture: sparc");
        assert_ne!(broken, rendered.installer_yaml, "mutation must apply");

        let value = yaml_to_json(&broken).expect("yaml parses");
        let findings = validate_json("winget", &value, INSTALLER_SCHEMA).expect("validation runs");

        let arch = findings
            .iter()
            .find(|f| f.field.ends_with("/Architecture"))
            .unwrap_or_else(|| {
                panic!("a finding for the out-of-enum Architecture field; got: {findings:?}")
            });
        assert_eq!(arch.publisher, "winget");
        assert!(
            arch.expected.contains("sparc") || arch.expected.contains("enum"),
            "expected message explains the enum violation, got: {}",
            arch.expected
        );
    }

    /// A required-key omission also bites: dropping `PackageIdentifier` from the
    /// version manifest is rejected at the document root.
    #[test]
    fn missing_required_key_is_reported() {
        let cfg = every_option_winget_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let rendered = render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let mut value = yaml_to_json(&rendered.version_yaml).expect("yaml parses");
        value
            .as_object_mut()
            .expect("version manifest is a map")
            .remove("PackageIdentifier");

        let findings = validate_json("winget", &value, VERSION_SCHEMA).expect("validation runs");
        assert!(
            findings
                .iter()
                .any(|f| f.expected.contains("PackageIdentifier")),
            "dropping a required key must be reported, got: {findings:?}"
        );
    }

    /// `resolve_winget_identity` is resolved exactly once per render, so the
    /// "publisher not explicitly set; falling back to repo owner" warning it
    /// emits when `publish.winget.publisher` is unset fires once — not twice.
    /// Pins the seam where a re-resolving renderer would double-warn.
    #[test]
    fn publisher_fallback_warning_fires_once_per_render() {
        // No explicit `publisher` — resolution falls back to the repo owner and
        // warns. Keep every other required field present so the render reaches
        // manifest generation.
        let cfg = WingetConfig {
            publisher: None,
            ..every_option_winget_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![winget_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_windows_zip(&mut ctx, "widget", "widget");

        let capture = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());

        render_winget_manifests_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");

        let fallbacks: Vec<String> = capture
            .warn_messages()
            .into_iter()
            .filter(|m| m.contains("falling back to repo owner"))
            .collect();
        assert_eq!(
            fallbacks.len(),
            1,
            "the publisher fallback warning must fire exactly once per render, got: {fallbacks:?}"
        );
    }
}
