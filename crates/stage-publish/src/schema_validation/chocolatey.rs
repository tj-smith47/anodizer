//! Chocolatey nuspec schema validation.
//!
//! A Chocolatey package's metadata lives in a NuGet `.nuspec` XML manifest,
//! whose shape the NuGet `nuspec.xsd` constrains: `<metadata>` is an `xs:all`
//! requiring `<id>`, `<version>`, `<authors>`, `<description>`, typing
//! `<requireLicenseAcceptance>` as `xs:boolean` and the url fields as
//! `xs:anyURI`. anodizer renders that nuspec per crate; this validator renders
//! the exact XML a live publish would stage — via the same render path — and
//! checks it two ways: an always-on structural floor (a real read-only XML
//! parse plus the required-child / namespace / boolean / no-duplicate rules)
//! and, when `xmllint` is on `PATH`, a full XSD validation against the vendored
//! schema. A structural defect (a missing required element, an XML-escaping
//! bug in user-supplied metadata, a wrong-typed boolean) surfaces in the
//! snapshot/dry-run pass rather than after a registry-rejected nupkg ships.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use super::{PublisherSchemaValidator, SchemaFinding, TagResolver, with_validated_crate_scope};
use crate::chocolatey::{is_chocolatey_per_crate_configured, render_nuspec_for_crate};

/// The NuGet nuspec XML Schema, with the `{0}` namespace placeholders replaced
/// by the `2015/06` namespace the renderer stamps. Pinned and embedded so the
/// `xmllint` layer is fully offline; refresh via `schemas/SOURCES.md`.
const NUSPEC_XSD: &str = include_str!("../../schemas/chocolatey.nuspec.xsd");

/// The XML namespace the rendered `.nuspec` declares on its root `<package>`.
/// Must equal the namespace substituted into the vendored XSD, or `xmllint`
/// would validate the manifest against an unrelated schema.
const NUSPEC_NAMESPACE: &str = "http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd";

/// `<metadata>` children the NuGet schema marks `minOccurs="1"` — each must be
/// present and non-empty, or the registry rejects the manifest.
const REQUIRED_METADATA_CHILDREN: &[&str] = &["id", "version", "authors", "description"];

/// Validates anodizer's rendered Chocolatey nuspecs against the NuGet schema.
pub(crate) struct ChocolateySchemaValidator;

impl PublisherSchemaValidator for ChocolateySchemaValidator {
    fn publisher(&self) -> &'static str {
        "chocolatey"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        // Chocolatey's community feed is a moderation one-way door: on the strict
        // pre-publish gate a missing `xmllint` must FAIL the floor, not skip it.
        let strict = ctx.render_is_strict();
        let mut findings = Vec::new();

        // Walk exactly the crate set the live chocolatey publisher iterates
        // (honoring `--crate` selection, else every chocolatey-configured
        // crate) so the validated set equals the published set in all config
        // modes.
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_chocolatey_per_crate_configured,
        );
        for crate_name in &selected {
            if !is_chocolatey_per_crate_configured(ctx, crate_name) {
                continue;
            }

            // Render + validate under THIS crate's own version (workspace
            // per-crate independent-version mode renders each crate's nuspec
            // `<version>` against its own version, not the first crate's).
            let crate_findings = with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                // `None` means the publisher would skip this crate (skip / falsy
                // `if`) — nothing to render or validate.
                let Some(nuspec) = render_nuspec_for_crate(ctx, crate_name, &log)? else {
                    return Ok(Vec::new());
                };
                let mut out = validate_nuspec_structural(&nuspec);
                out.extend(validate_nuspec_xmllint(&nuspec, strict, &log)?);
                Ok(out)
            })?;
            findings.extend(crate_findings);
        }

        Ok(findings)
    }
}

/// The always-on, hermetic structural floor: parse the rendered nuspec with a
/// real read-only XML parser and assert the rules the NuGet schema enforces
/// that a registry rejection would otherwise be the first to surface. Returns
/// one [`SchemaFinding`] per violation; an empty Vec means the document clears
/// the floor. Runs with no external tools, so it holds even where `xmllint` is
/// absent.
pub(crate) fn validate_nuspec_structural(xml: &str) -> Vec<SchemaFinding> {
    let finding = |field: &str, expected: &str| SchemaFinding {
        publisher: "chocolatey".to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
    };

    // A parse error is itself a finding — this is what catches an XML-escaping
    // bug in user-supplied metadata (an unescaped `&`, `<`, …).
    let doc = match roxmltree::Document::parse(xml) {
        Ok(doc) => doc,
        Err(e) => {
            return vec![finding(
                "(root)",
                &format!("well-formed XML per nuspec.xsd; parse error: {e}"),
            )];
        }
    };

    let mut findings = Vec::new();
    let package = doc.root_element();

    if package.tag_name().namespace() != Some(NUSPEC_NAMESPACE) {
        findings.push(finding(
            "package/@xmlns",
            &format!("root namespace must be '{NUSPEC_NAMESPACE}'"),
        ));
    }

    let Some(metadata) = package
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "metadata")
    else {
        findings.push(finding(
            "metadata",
            "nuspec.xsd requires a <metadata> element (minOccurs=1)",
        ));
        return findings;
    };

    // Collect metadata children once: drives the required/non-empty check, the
    // `xs:all` no-duplicate check, and the boolean-type check.
    let meta_elems: Vec<roxmltree::Node<'_, '_>> =
        metadata.children().filter(|n| n.is_element()).collect();

    for &required in REQUIRED_METADATA_CHILDREN {
        let value = meta_elems
            .iter()
            .find(|n| n.tag_name().name() == required)
            .and_then(|n| n.text())
            .map(str::trim)
            .unwrap_or("");
        if value.is_empty() {
            findings.push(finding(
                &format!("metadata/{required}"),
                &format!("nuspec.xsd requires a non-empty <{required}> (minOccurs=1)"),
            ));
        }
    }

    // `<requireLicenseAcceptance>` is `xs:boolean` — exactly `true` or `false`.
    if let Some(rla) = meta_elems
        .iter()
        .find(|n| n.tag_name().name() == "requireLicenseAcceptance")
    {
        let value = rla.text().map(str::trim).unwrap_or("");
        // xs:boolean's lexical space is exactly {true, false, 1, 0}; accept all
        // four so the floor agrees with what `xmllint --schema` admits.
        if !matches!(value, "true" | "false" | "1" | "0") {
            findings.push(finding(
                "metadata/requireLicenseAcceptance",
                "nuspec.xsd types <requireLicenseAcceptance> as xs:boolean (true|false|1|0)",
            ));
        }
    }

    // `<metadata>` is an `xs:all`: each child may appear at most once.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for elem in &meta_elems {
        let name = elem.tag_name().name();
        if !seen.insert(name) {
            findings.push(finding(
                &format!("metadata/{name}"),
                "nuspec.xsd declares <metadata> as xs:all; each child may appear at most once",
            ));
        }
    }

    findings
}

/// The gated, belt-and-suspenders layer: when `xmllint` is on `PATH`, write
/// the vendored XSD and the rendered nuspec to a tempdir and run
/// `xmllint --noout --schema <xsd> <nuspec>`, parsing each
/// `Schemas validity error` stderr line into a [`SchemaFinding`]. When
/// `xmllint` is absent and the floor is lenient (local check / dry-run), log a
/// note and return no findings — the structural floor stands. On the STRICT
/// pre-publish gate, an absent `xmllint` instead fails the floor: chocolatey is
/// a moderation one-way door, so a missing XSD layer there would let a malformed
/// nuspec reach the irreversible queue unchecked (`tool_required: true`).
fn validate_nuspec_xmllint(
    xml: &str,
    strict: bool,
    log: &StageLogger,
) -> Result<Vec<SchemaFinding>> {
    super::run_external_validator(
        &super::ExternalValidator {
            publisher: "chocolatey",
            tool: "xmllint",
            // Schema first, then the document — matches `--schema <xsd> <nuspec>`.
            flags: &["--noout", "--schema"],
            files: &[
                ("chocolatey.nuspec.xsd", NUSPEC_XSD),
                ("package.nuspec", xml),
            ],
            skip_message: "xmllint not on PATH — relying on the structural nuspec floor \
                 for schema validation",
            empty_fallback: "xmllint reported the nuspec schema-invalid but emitted no parseable validity line",
            tool_required: true,
        },
        parse_xmllint_stderr,
        log,
        strict,
    )
}

/// Parse `xmllint --schema` stderr into [`SchemaFinding`]s. Each validity
/// error line has the shape
/// `<file>:<line>: element <ctx>: Schemas validity error : <msg>`; the element
/// context becomes the finding field and the message its expectation. Lines
/// without that marker (e.g. the trailing `<file> fails to validate`) are
/// ignored.
fn parse_xmllint_stderr(stderr: &str) -> Vec<SchemaFinding> {
    const MARKER: &str = "Schemas validity error :";
    stderr
        .lines()
        .filter_map(|line| {
            let (prefix, msg) = line.split_once(MARKER)?;
            let field = prefix
                .rsplit_once("element ")
                .map(|(_, ctx)| ctx.trim().trim_end_matches(':').trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("(root)")
                .to_string();
            Some(SchemaFinding {
                publisher: "chocolatey".to_string(),
                field,
                expected: msg.trim().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use anodizer_core::config::{
        ChocolateyConfig, ChocolateyDependency, CrateConfig, PublishConfig, ReleaseConfig,
        RepositoryConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;

    /// A `ChocolateyConfig` exercising every nuspec-affecting option, with
    /// values the NuGet schema accepts (SPDX license, http URLs, typed lists).
    fn every_option_choco_cfg() -> ChocolateyConfig {
        ChocolateyConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("widget".to_string()),
                ..Default::default()
            }),
            package_source_url: Some("https://acme.example/src".to_string()),
            owners: Some("acme".to_string()),
            title: Some("Widget".to_string()),
            authors: Some("Acme Corp".to_string()),
            project_url: Some("https://acme.example/widget".to_string()),
            icon_url: Some("https://acme.example/icon.png".to_string()),
            copyright: Some("Copyright 2026 Acme".to_string()),
            description: Some("A widget management tool".to_string()),
            license: Some("MIT".to_string()),
            license_url: Some("https://acme.example/license".to_string()),
            require_license_acceptance: Some(true),
            project_source_url: Some("https://github.com/acme/widget".to_string()),
            docs_url: Some("https://acme.example/docs".to_string()),
            bug_tracker_url: Some("https://acme.example/bugs".to_string()),
            tags: Some(vec!["cli".to_string(), "tool".to_string()]),
            summary: Some("Widget summary".to_string()),
            release_notes: Some("Initial release".to_string()),
            dependencies: Some(vec![ChocolateyDependency {
                id: "chocolatey-core.extension".to_string(),
                version: Some("[1.0.0,)".to_string()),
            }]),
            ..Default::default()
        }
    }

    fn choco_crate(crate_name: &str, tag_template: &str, cfg: ChocolateyConfig) -> CrateConfig {
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
                chocolatey: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Re-scope the global template vars to the version a release would stamp,
    /// the same shape the publish stage applies before invoking a per-crate
    /// publisher.
    fn scope_version(ctx: &mut Context, version: &str) {
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("RawVersion", version);
        ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    }

    /// Parse a rendered nuspec and return its `<metadata>` node's child text by
    /// element name, for asserting each option lands in the schema-expected
    /// element.
    fn meta_text<'a>(doc: &'a roxmltree::Document<'a>, name: &str) -> Option<String> {
        doc.root_element()
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "metadata")?
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == name)?
            .text()
            .map(str::to_string)
    }

    /// (a) Single-crate mode: one crate, every option set. The rendered nuspec
    /// must clear the structural floor with zero findings and land each option
    /// in its schema-expected element.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_fields() {
        let cfg = every_option_choco_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let findings = ChocolateySchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate nuspec must conform, got: {findings:?}"
        );

        let nuspec = render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let doc = roxmltree::Document::parse(&nuspec).expect("rendered nuspec parses");

        assert_eq!(
            doc.root_element().tag_name().namespace(),
            Some(NUSPEC_NAMESPACE)
        );
        assert_eq!(meta_text(&doc, "id").as_deref(), Some("widget"));
        assert_eq!(meta_text(&doc, "version").as_deref(), Some("1.0.0"));
        assert_eq!(meta_text(&doc, "authors").as_deref(), Some("Acme Corp"));
        assert_eq!(meta_text(&doc, "title").as_deref(), Some("Widget"));
        assert_eq!(
            meta_text(&doc, "description").as_deref(),
            Some("A widget management tool")
        );
        assert_eq!(meta_text(&doc, "owners").as_deref(), Some("acme"));
        assert_eq!(
            meta_text(&doc, "packageSourceUrl").as_deref(),
            Some("https://acme.example/src")
        );
        assert_eq!(
            meta_text(&doc, "projectUrl").as_deref(),
            Some("https://acme.example/widget")
        );
        assert_eq!(
            meta_text(&doc, "iconUrl").as_deref(),
            Some("https://acme.example/icon.png")
        );
        assert_eq!(
            meta_text(&doc, "licenseUrl").as_deref(),
            Some("https://acme.example/license")
        );
        assert_eq!(
            meta_text(&doc, "requireLicenseAcceptance").as_deref(),
            Some("true")
        );
        assert_eq!(
            meta_text(&doc, "copyright").as_deref(),
            Some("Copyright 2026 Acme")
        );
        assert_eq!(
            meta_text(&doc, "projectSourceUrl").as_deref(),
            Some("https://github.com/acme/widget")
        );
        assert_eq!(
            meta_text(&doc, "docsUrl").as_deref(),
            Some("https://acme.example/docs")
        );
        assert_eq!(
            meta_text(&doc, "bugTrackerUrl").as_deref(),
            Some("https://acme.example/bugs")
        );
        assert_eq!(meta_text(&doc, "tags").as_deref(), Some("cli tool"));
        assert_eq!(
            meta_text(&doc, "summary").as_deref(),
            Some("Widget summary")
        );
        assert_eq!(
            meta_text(&doc, "releaseNotes").as_deref(),
            Some("Initial release")
        );

        // The dependency lands as `metadata/dependencies/dependency/@id`.
        let dep_id = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "dependency")
            .and_then(|n| n.attribute("id"));
        assert_eq!(dep_id, Some("chocolatey-core.extension"));
    }

    /// (b) Workspace-lockstep mode: multiple crates share one global version.
    /// Each crate's nuspec must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = choco_crate(
            "alpha",
            "v{{ .Version }}",
            ChocolateyConfig {
                name: Some("alpha".to_string()),
                ..every_option_choco_cfg()
            },
        );
        let beta = choco_crate(
            "beta",
            "v{{ .Version }}",
            ChocolateyConfig {
                name: Some("beta".to_string()),
                ..every_option_choco_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let findings = ChocolateySchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace nuspecs must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate mode: each crate carries its own tag_template /
    /// version. The publish stage scopes the global `Version` to the per-crate
    /// value before invoking the publisher, so the validator (run per-crate via
    /// `--crate`) must conform — and stamp its own `<version>` — under each
    /// crate's own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = choco_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            ChocolateyConfig {
                name: Some("alpha".to_string()),
                ..every_option_choco_cfg()
            },
        );
        let beta = choco_crate(
            "beta",
            "beta-v{{ .Version }}",
            ChocolateyConfig {
                name: Some("beta".to_string()),
                ..every_option_choco_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        let findings_a = ChocolateySchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let nuspec_a = render_nuspec_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let doc_a = roxmltree::Document::parse(&nuspec_a).expect("nuspec parses");
        assert_eq!(meta_text(&doc_a, "version").as_deref(), Some("2.0.0"));

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        let findings_b = ChocolateySchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let nuspec_b = render_nuspec_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let doc_b = roxmltree::Document::parse(&nuspec_b).expect("nuspec parses");
        assert_eq!(meta_text(&doc_b, "version").as_deref(), Some("3.1.0"));
    }

    /// A crate whose `if:` renders falsy must be skipped: `render_nuspec_for_crate`
    /// returns `None` and the validator yields no findings for it.
    #[test]
    fn crate_with_falsy_if_is_skipped() {
        let cfg = ChocolateyConfig {
            if_condition: Some("false".to_string()),
            ..every_option_choco_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let rendered =
            render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish")).expect("render ok");
        assert!(
            rendered.is_none(),
            "a falsy `if` must skip the crate, got a rendered nuspec"
        );

        let findings = ChocolateySchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a skipped crate yields no findings, got: {findings:?}"
        );
    }

    /// A choco config that sets `repository` but leaves `license_url`,
    /// `project_source_url`, and `bug_tracker_url` UNSET — exercising the
    /// derived-default path. `license` is a single SPDX identifier so the
    /// derived `<licenseUrl>` (a GitHub LICENSE blob URL) is emitted, proving
    /// it is derived rather than synthesizing a 404ing `opensource.org` URL.
    /// (A compound SPDX expression suppresses `<licenseUrl>` entirely —
    /// covered by the `publish::tests` unit tests.)
    fn derive_defaults_choco_cfg(pkg: &str) -> ChocolateyConfig {
        ChocolateyConfig {
            name: Some(pkg.to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some(pkg.to_string()),
                ..Default::default()
            }),
            authors: Some("Acme Corp".to_string()),
            description: Some("A widget management tool".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        }
    }

    /// (a) Single-crate mode, derived defaults: the SPDX expression lands in
    /// `<license type="expression">`, `<licenseUrl>` is the derived GitHub
    /// LICENSE blob URL pinned at the tag, `<projectSourceUrl>` is the repo
    /// URL, `<bugTrackerUrl>` is `{repo}/issues` — and NO opensource.org URL
    /// is ever synthesized.
    #[test]
    fn single_crate_derives_license_expr_and_repo_urls() {
        let cfg = derive_defaults_choco_cfg("widget");
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");

        let findings = ChocolateySchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "derived nuspec must conform: {findings:?}"
        );

        let nuspec = render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            !nuspec.contains("opensource.org"),
            "must never synthesize an opensource.org licenseUrl: {nuspec}"
        );
        assert!(
            nuspec.contains("<license type=\"expression\">MIT</license>"),
            "SPDX license must land as a license expression: {nuspec}"
        );
        assert!(
            nuspec.contains(
                "<licenseUrl>https://github.com/acme/widget/blob/v1.0.0/LICENSE</licenseUrl>"
            ),
            "licenseUrl must be the derived GitHub blob URL pinned at the tag: {nuspec}"
        );
        assert!(
            nuspec.contains("<projectSourceUrl>https://github.com/acme/widget</projectSourceUrl>"),
            "projectSourceUrl must derive from the repo: {nuspec}"
        );
        assert!(
            nuspec.contains("<bugTrackerUrl>https://github.com/acme/widget/issues</bugTrackerUrl>"),
            "bugTrackerUrl must derive as {{repo}}/issues: {nuspec}"
        );
    }

    /// (c) Workspace per-crate mode, derived defaults: each crate's nuspec
    /// resolves ITS OWN repo + version. alpha's licenseUrl/projectSourceUrl
    /// must point at acme/alpha (not beta), pinned at alpha's own tag — proving
    /// the derived fields resolve per published crate, not last-writer-wins.
    #[test]
    fn workspace_per_crate_derives_fields_per_crate() {
        let alpha = choco_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            derive_defaults_choco_cfg("alpha"),
        );
        let beta = choco_crate(
            "beta",
            "beta-v{{ .Version }}",
            derive_defaults_choco_cfg("beta"),
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        let nuspec_a = render_nuspec_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            nuspec_a.contains(
                "<licenseUrl>https://github.com/acme/alpha/blob/v2.0.0/LICENSE</licenseUrl>"
            ),
            "alpha licenseUrl must be acme/alpha at its own tag: {nuspec_a}"
        );
        assert!(
            nuspec_a.contains("<projectSourceUrl>https://github.com/acme/alpha</projectSourceUrl>"),
            "alpha projectSourceUrl must be acme/alpha: {nuspec_a}"
        );
        assert!(
            nuspec_a
                .contains("<bugTrackerUrl>https://github.com/acme/alpha/issues</bugTrackerUrl>"),
            "alpha bugTrackerUrl must be acme/alpha/issues: {nuspec_a}"
        );
        assert!(
            !nuspec_a.contains("acme/beta"),
            "alpha must not leak beta's repo"
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        let nuspec_b = render_nuspec_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            nuspec_b.contains(
                "<licenseUrl>https://github.com/acme/beta/blob/v3.1.0/LICENSE</licenseUrl>"
            ),
            "beta licenseUrl must be acme/beta at its own tag: {nuspec_b}"
        );
        assert!(
            !nuspec_b.contains("acme/alpha"),
            "beta must not leak alpha's repo"
        );
    }

    /// An explicit `license_url` / `project_source_url` / `bug_tracker_url`
    /// always wins over the derived default (override semantics).
    #[test]
    fn explicit_urls_override_derived_defaults() {
        let cfg = ChocolateyConfig {
            license_url: Some("https://acme.example/license".to_string()),
            project_source_url: Some("https://acme.example/src".to_string()),
            bug_tracker_url: Some("https://acme.example/bugs".to_string()),
            ..derive_defaults_choco_cfg("widget")
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        let nuspec = render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(nuspec.contains("<licenseUrl>https://acme.example/license</licenseUrl>"));
        assert!(nuspec.contains("<projectSourceUrl>https://acme.example/src</projectSourceUrl>"));
        assert!(nuspec.contains("<bugTrackerUrl>https://acme.example/bugs</bugTrackerUrl>"));
        assert!(
            !nuspec.contains("/blob/"),
            "explicit licenseUrl must not be overridden by blob derivation"
        );
    }

    /// An internal feed (no `repository` configured) derives NO repo URLs and
    /// NO licenseUrl — the SPDX `<license type="expression">` still ships, but
    /// no fabricated URL does. Confirms the derivation is skipped, not 404'd.
    #[test]
    fn no_repo_omits_derived_urls_keeps_license_expression() {
        let cfg = ChocolateyConfig {
            name: Some("widget".to_string()),
            authors: Some("Acme".to_string()),
            description: Some("A widget".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        let nuspec = render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(nuspec.contains("<license type=\"expression\">MIT</license>"));
        assert!(
            !nuspec.contains("<licenseUrl>"),
            "no licenseUrl without a repo: {nuspec}"
        );
        assert!(
            !nuspec.contains("<projectUrl>"),
            "no projectUrl without a repo"
        );
        assert!(!nuspec.contains("<projectSourceUrl>"));
        assert!(!nuspec.contains("<bugTrackerUrl>"));
        assert!(!nuspec.contains("opensource.org"));
    }

    /// Compound SPDX expression WITH a repo present: the rendered nuspec must
    /// carry the full expression in `<license type="expression">` and must NOT
    /// emit a `<licenseUrl>` element. The repo is set so a single-identifier
    /// license WOULD derive a `<licenseUrl>` — proving the suppression is
    /// driven by the compound expression, not by an absent repo. Locks the
    /// emitted manifest XML, not just the intermediate struct.
    #[test]
    fn compound_spdx_rendered_nuspec_has_expression_and_no_license_url() {
        let cfg = ChocolateyConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("widget".to_string()),
                ..Default::default()
            }),
            authors: Some("Acme Corp".to_string()),
            description: Some("A widget management tool".to_string()),
            license: Some("MIT OR Apache-2.0".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        let nuspec = render_nuspec_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            nuspec.contains("<license type=\"expression\">MIT OR Apache-2.0</license>"),
            "compound SPDX must land as a license expression: {nuspec}"
        );
        assert!(
            !nuspec.contains("<licenseUrl>"),
            "compound SPDX must NOT emit a <licenseUrl> even with a repo present: {nuspec}"
        );
        assert!(
            !nuspec.contains("opensource.org"),
            "must never synthesize an opensource.org licenseUrl: {nuspec}"
        );
        // The repo-derived sibling URLs still emit — only <licenseUrl> is gated.
        assert!(
            nuspec.contains("<projectSourceUrl>https://github.com/acme/widget</projectSourceUrl>"),
            "projectSourceUrl must still derive from the repo: {nuspec}"
        );
    }

    /// The structural floor must BITE: an empty `<authors>` (a required,
    /// non-empty element per nuspec.xsd) is rejected with a finding naming the
    /// field. The corrected document produces zero findings, proving the test
    /// bites rather than always-failing.
    #[test]
    fn empty_required_element_is_reported_and_fix_clears_it() {
        let broken = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>widget</id>
    <version>1.0.0</version>
    <authors></authors>
    <description>A widget</description>
  </metadata>
</package>
"#;
        let findings = validate_nuspec_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "metadata/authors"),
            "empty <authors> must be reported, got: {findings:?}"
        );

        let fixed = broken.replace("<authors></authors>", "<authors>Acme</authors>");
        let fixed_findings = validate_nuspec_structural(&fixed);
        assert!(
            fixed_findings.is_empty(),
            "the corrected nuspec must produce zero findings, got: {fixed_findings:?}"
        );
    }

    /// Malformed XML — an unescaped `&` in user-supplied metadata — is itself a
    /// finding (the parse fails), which is exactly what an XML-escaping bug
    /// would surface as.
    #[test]
    fn malformed_xml_is_reported_at_root() {
        let broken = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>widget</id>
    <version>1.0.0</version>
    <authors>Acme & Co</authors>
    <description>A widget</description>
  </metadata>
</package>
"#;
        let findings = validate_nuspec_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "(root)"),
            "unescaped `&` must produce a root parse finding, got: {findings:?}"
        );
    }

    /// A wrong-typed `<requireLicenseAcceptance>` (an `xs:boolean` set to a
    /// non-boolean) bites the structural floor.
    #[test]
    fn non_boolean_require_license_acceptance_is_reported() {
        let broken = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>widget</id>
    <version>1.0.0</version>
    <authors>Acme</authors>
    <description>A widget</description>
    <requireLicenseAcceptance>yes</requireLicenseAcceptance>
  </metadata>
</package>
"#;
        let findings = validate_nuspec_structural(broken);
        assert!(
            findings
                .iter()
                .any(|f| f.field == "metadata/requireLicenseAcceptance"),
            "a non-boolean requireLicenseAcceptance must be reported, got: {findings:?}"
        );

        // The full xs:boolean lexical space is {true, false, 1, 0}; each must
        // clear the floor so it agrees with what the XSD admits.
        for boolean in ["true", "false", "1", "0"] {
            let ok = broken.replace(
                "<requireLicenseAcceptance>yes</requireLicenseAcceptance>",
                &format!("<requireLicenseAcceptance>{boolean}</requireLicenseAcceptance>"),
            );
            let ok_findings = validate_nuspec_structural(&ok);
            assert!(
                !ok_findings
                    .iter()
                    .any(|f| f.field == "metadata/requireLicenseAcceptance"),
                "xs:boolean value '{boolean}' must clear the floor, got: {ok_findings:?}"
            );
        }
    }

    /// `<metadata>` is an `xs:all`: a duplicated `<version>` is rejected.
    #[test]
    fn duplicated_metadata_child_is_reported() {
        let broken = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>widget</id>
    <version>1.0.0</version>
    <version>2.0.0</version>
    <authors>Acme</authors>
    <description>A widget</description>
  </metadata>
</package>
"#;
        let findings = validate_nuspec_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "metadata/version"),
            "a duplicated <version> must be reported, got: {findings:?}"
        );
    }

    /// The xmllint stderr parser maps a `Schemas validity error` line to a
    /// finding whose field is the element context and whose expectation is the
    /// validity message. Holds even where the tool itself is absent.
    #[test]
    fn xmllint_stderr_parses_into_findings() {
        let stderr = "package.nuspec:5: element authors: Schemas validity error : \
                      Element 'authors': [facet 'minLength'] The value '' is too short.\n\
                      package.nuspec fails to validate\n";
        let findings = parse_xmllint_stderr(stderr);
        assert_eq!(
            findings.len(),
            1,
            "one validity error line, got: {findings:?}"
        );
        assert_eq!(findings[0].publisher, "chocolatey");
        assert_eq!(findings[0].field, "authors");
        assert!(
            findings[0].expected.contains("too short"),
            "expectation carries the validity message, got: {}",
            findings[0].expected
        );
    }

    /// The full XSD layer must accept the every-option nuspec: render it and run
    /// it through the REAL `xmllint --schema` against the vendored XSD, asserting
    /// zero findings. This is the test that proves the vendored schema actually
    /// admits the Chocolatey gallery extensions (`packageSourceUrl`,
    /// `projectSourceUrl`, `docsUrl`, `bugTrackerUrl`) the renderer emits — the
    /// base NuGet `<xs:all>` does not, so without the augmentation this fails.
    /// Skipped (with a visible marker) when `xmllint` is not on `PATH`.
    #[test]
    fn xmllint_accepts_every_option_nuspec() {
        let cfg = every_option_choco_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![choco_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        let log = ctx.logger("publish");

        match anodizer_core::tool_detect::runs("xmllint") {
            anodizer_core::tool_detect::ToolProbe::Available => {}
            anodizer_core::tool_detect::ToolProbe::Unavailable => {
                log.status(
                    "SKIP xmllint_accepts_every_option_nuspec: xmllint not on PATH (XSD layer unexercised)",
                );
                return;
            }
            anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
                log.status(&format!(
                    "SKIP xmllint_accepts_every_option_nuspec: xmllint probe failed ({e}) (XSD layer unexercised)"
                ));
                return;
            }
        }

        let nuspec = render_nuspec_for_crate(&ctx, "widget", &log)
            .expect("render ok")
            .expect("not skipped");
        let findings = validate_nuspec_xmllint(&nuspec, false, &log).expect("xmllint runs");
        assert!(
            findings.is_empty(),
            "the every-option nuspec must validate against the vendored XSD, got: {findings:?}"
        );
    }

    /// The XSD layer must BITE: a nuspec carrying an element the schema does not
    /// define (`<notAThing>`) must produce a finding from `xmllint --schema`,
    /// proving the full-XSD pass catches what the structural floor does not.
    /// Skipped (with a visible marker) when `xmllint` is not on `PATH`.
    #[test]
    fn xmllint_rejects_unknown_element() {
        let ctx = TestContextBuilder::new().snapshot(true).build();
        let log = ctx.logger("publish");

        match anodizer_core::tool_detect::runs("xmllint") {
            anodizer_core::tool_detect::ToolProbe::Available => {}
            anodizer_core::tool_detect::ToolProbe::Unavailable => {
                log.status(
                    "SKIP xmllint_rejects_unknown_element: xmllint not on PATH (XSD layer unexercised)",
                );
                return;
            }
            anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
                log.status(&format!(
                    "SKIP xmllint_rejects_unknown_element: xmllint probe failed ({e}) (XSD layer unexercised)"
                ));
                return;
            }
        }

        let invalid = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata>
    <id>widget</id>
    <version>1.0.0</version>
    <authors>Acme</authors>
    <description>A widget</description>
    <notAThing>unexpected</notAThing>
  </metadata>
</package>
"#;
        let findings = validate_nuspec_xmllint(invalid, false, &log).expect("xmllint runs");
        assert!(
            !findings.is_empty(),
            "an unknown element must be rejected by the XSD layer, got no findings"
        );
        assert!(
            findings.iter().any(|f| f.expected.contains("notAThing")
                || f.field.contains("notAThing")
                || f.expected.contains("not expected")),
            "the finding must name the offending element, got: {findings:?}"
        );
    }
}
