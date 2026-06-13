//! Nix `default.nix` derivation + root `flake.nix` structural validation.
//!
//! A Nix overlay repo has no JSON/YAML schema: a `default.nix` is a function
//! producing a `stdenvNoCC.mkDerivation { … }` attrset, and the root
//! `flake.nix` is a `{ description; inputs; outputs; }` expression. `nix-build`
//! / `nix flake check` accept them only when the expression parses and carries
//! the load-bearing attributes — a derivation needs `pname` / `version` / a
//! `src = fetchurl { url …; sha256 …; }` / a `meta` attrset / the
//! `mkDerivation` call; a flake needs `description` / `inputs` / `outputs` and
//! overlay `callPackage` lines that round-trip. anodizer renders that pair per
//! nix-configured crate (the crate's `default.nix`) plus the merged root
//! `flake.nix`; this validator renders the exact expressions a live publish
//! would push — via the same render path — and checks them two ways: an
//! always-on structural floor (pure-Rust, string/comment-aware delimiter
//! balance + required-attr scanning) and, when `nix-instantiate` is on `PATH`,
//! a real `nix-instantiate --parse` of each file. A structural defect (a
//! missing required attribute, an unbalanced template that produced broken
//! Nix) surfaces in the snapshot/dry-run pass rather than after a pushed
//! overlay fails `nix build` for every installer.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use super::{PublisherSchemaValidator, SchemaFinding, TagResolver, with_validated_crate_scope};
use crate::nix::{
    self, FlakePackage, crate_has_nix_archive, is_nix_per_crate_configured,
    render_nix_for_validation,
};

/// Which kind of artifact a rendered Nix string is — drives the attribute set
/// the structural floor asserts.
#[derive(Clone, Copy)]
enum NixKind {
    Derivation,
    Flake,
}

/// Validates anodizer's rendered Nix `default.nix` derivation + root
/// `flake.nix`.
pub(crate) struct NixSchemaValidator;

impl PublisherSchemaValidator for NixSchemaValidator {
    fn publisher(&self) -> &'static str {
        "nix"
    }

    fn validate(
        &self,
        ctx: &mut Context,
        resolve_tag: TagResolver<'_>,
    ) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk exactly the crate set the live nix publisher's `run` iterates
        // (honoring `--crate` selection, else every nix-configured crate) so
        // the validated set equals the published set in all config modes.
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_nix_per_crate_configured);

        // The flake the next publish writes merges every package into one root
        // `flake.nix`; accumulate each rendered crate's package entry so the
        // single merged flake is validated once below. The flake itself carries
        // no version, so it is validated under the global scope; only the
        // per-crate DERIVATION (which carries `version`) is rendered under the
        // crate's own version scope.
        let mut flake_pkgs: Vec<FlakePackage> = Vec::new();

        for crate_name in &selected {
            if !is_nix_per_crate_configured(ctx, crate_name) {
                continue;
            }

            // Render + validate the per-crate derivation under THIS crate's own
            // version (workspace per-crate independent-version mode renders each
            // crate's `version` against its own version, not the first crate's),
            // returning the flake package entry for the post-loop merged flake.
            let (crate_findings, flake_pkg) =
                with_validated_crate_scope(ctx, crate_name, resolve_tag, |ctx| {
                    let mut out = Vec::new();
                    let nix_cfg = crate::util::all_crates(ctx)
                        .into_iter()
                        .find(|c| &c.name == crate_name)
                        .and_then(|c| c.publish)
                        .and_then(|p| p.nix);

                    // A real release always builds at least one archive the
                    // derivation `src` points at, but a sharded / single-target
                    // snapshot may build none for this crate. The probe
                    // distinguishes ABSENCE from ERROR: a clean `Ok(false)` (no
                    // Nix-mappable artifact) skips, while a matched-but-broken
                    // artifact (missing sha256) propagates as `Err` via the `?` —
                    // the same defect the live publish path bails on — rather than
                    // being silently skipped.
                    if let Some(nix_cfg) = nix_cfg.as_ref()
                        && !crate_has_nix_archive(ctx, nix_cfg, crate_name)?
                    {
                        log.verbose(&format!(
                            "skipped derivation schema validation for crate '{}' — produced no \
                             Nix-mappable archive in this snapshot shard",
                            crate_name
                        ));
                        return Ok((out, None));
                    }

                    // `render_nix_for_validation` returns `None` when the
                    // publisher would skip (skip / `if` falsy / skip_upload), so a
                    // skipped emission is nothing to validate rather than a
                    // failure.
                    let Some(render) = render_nix_for_validation(ctx, crate_name, &log)? else {
                        log.verbose(&format!(
                            "crate '{}' nix publisher would skip; nothing to validate",
                            crate_name
                        ));
                        return Ok((out, None));
                    };

                    out.extend(validate_derivation_structural(&render.expr));
                    out.extend(validate_nix_syntax(
                        NixKind::Derivation,
                        &render.expr,
                        &log,
                    )?);

                    // The flake exposes this package via the default derivation
                    // path; the live `write_flake` honors a custom `nix.path`, but
                    // the validated flake only needs a path that round-trips
                    // through the overlay-line recovery parser, so the default
                    // path is sufficient.
                    let flake_pkg = FlakePackage {
                        attr: render.name.clone(),
                        path: format!("pkgs/{}/default.nix", render.name),
                    };
                    Ok((out, Some(flake_pkg)))
                })?;
            findings.extend(crate_findings);
            if let Some(pkg) = flake_pkg {
                flake_pkgs.push(pkg);
            }
        }

        // The merged root flake exposing every rendered package. Skipped when
        // no crate rendered (every crate skipped / sharded out) — an empty
        // flake is a degenerate case the live path never pushes.
        if !flake_pkgs.is_empty() {
            let flake = nix::generate_flake(&flake_pkgs)?;
            findings.extend(validate_flake_structural(&flake));
            findings.extend(validate_nix_syntax(NixKind::Flake, &flake, &log)?);
        }

        Ok(findings)
    }
}

fn finding(field: &str, expected: &str) -> SchemaFinding {
    SchemaFinding {
        publisher: "nix".to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
    }
}

/// True when `text` carries a line that, after trimming leading whitespace,
/// begins with `<attr> =` — the shape of a Nix attribute binding. The `=`
/// anchor (a bound value, not a bare substring) keeps a `version` mention
/// inside a string or comment from satisfying the `version =` requirement.
fn has_attr_binding(text: &str, attr: &str) -> bool {
    let prefix = format!("{attr} =");
    text.lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

/// True when `text` carries a non-empty string binding `<attr> = "<value>"`
/// whose value (the run up to the closing quote) is non-empty — used for the
/// `pname` / `version` attributes a derivation must stamp non-empty.
fn has_nonempty_string_attr(text: &str, attr: &str) -> bool {
    let prefix = format!("{attr} = \"");
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once('"'))
            .is_some_and(|(value, _)| !value.is_empty())
    })
}

/// The always-on, hermetic structural floor for a rendered `default.nix`
/// derivation: balanced delimiters (string/comment-aware) plus the attributes
/// `nix-build` HARD-requires, one [`SchemaFinding`] per violation. An empty Vec
/// means the expression clears the floor. Runs with no external tools, so it
/// holds even where `nix-instantiate` is absent.
///
/// The rendered output is template-controlled, so targeted line scanning is
/// sufficient — this is deliberately NOT a Nix parser. It is LENIENT about
/// optional attributes: `buildInputs` / `nativeBuildInputs` / `installPhase` /
/// `dontUnpack` / `sourceRoot` are all template-gated and a derivation without
/// them is valid, so requiring them would false-reject valid output — itself a
/// defect. `meta.description` and `meta.license` are likewise gated
/// (`{% if description %}` / `{% if license %}`): a metadata-light derivation
/// is valid Nix and installable, so the floor asserts the `meta` attrset is
/// PRESENT but does not require those optional sub-attributes.
pub(crate) fn validate_derivation_structural(text: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();

    // Balanced delimiters — `nix-build` rejects an unbalanced expression
    // outright. String/comment-aware so a literal brace inside the
    // `installPhase = ''…''` body or a `meta.description` string is not
    // miscounted.
    if let Err(e) = nix::nix_delimiters_balanced(text) {
        findings.push(finding("(root)", &e.to_string()));
    }

    // `stdenvNoCC.mkDerivation { … }` / `stdenv.mkDerivation { … }` — the call
    // that actually builds the package. Keyed on `mkDerivation` so either
    // stdenv form counts.
    if !text.contains("mkDerivation") {
        findings.push(finding(
            "mkDerivation",
            "a derivation must call `stdenv*.mkDerivation { … }`",
        ));
    }

    // `pname` / `version` — the package identity. Empty either yields an
    // unnameable / unversioned store path.
    for attr in ["pname", "version"] {
        if !has_nonempty_string_attr(text, attr) {
            findings.push(finding(
                attr,
                &format!("a derivation must carry a non-empty `{attr} = \"…\"` attribute"),
            ));
        }
    }

    // `src = …` with a `fetchurl` fetcher, a URL, and an integrity hash. The
    // template emits `src = fetchurl { url = selectSystem urlMap; sha256 =
    // selectSystem shaMap; }` over a `urlMap`/`shaMap` keyed by Nix system —
    // so assert the binding, the fetcher, a `url`, and a `sha256`/`hash`
    // independently (each is load-bearing: without the url there is nothing to
    // fetch, without the hash the fixed-output derivation cannot verify).
    if !has_attr_binding(text, "src") {
        findings.push(finding("src", "a derivation must bind `src = …`"));
    }
    if !text.contains("fetchurl") && !text.contains("fetchzip") {
        findings.push(finding(
            "src",
            "a derivation `src` must use a fetcher (`fetchurl`/`fetchzip`)",
        ));
    }
    if !has_attr_binding(text, "url") {
        findings.push(finding(
            "url",
            "a derivation `src` must bind a `url = …` to fetch",
        ));
    }
    // The integrity hash attribute — `sha256` (the template's form) or the
    // newer SRI `hash`. At least one must bind, or the fixed-output derivation
    // has nothing to verify the source against and `nix-build` rejects it.
    if !has_attr_binding(text, "sha256") && !has_attr_binding(text, "hash") {
        findings.push(finding(
            "sha256",
            "a derivation `src` must bind an integrity hash (`sha256 = …` or `hash = …`)",
        ));
    }

    // `meta = { … }` — the metadata attrset (`platforms` / `sourceProvenance`
    // are unconditional, so a well-formed derivation always emits it).
    // Lenient on its OPTIONAL sub-attributes (`description` / `license` are
    // template-gated and a derivation without them is valid).
    if !has_attr_binding(text, "meta") {
        findings.push(finding(
            "meta",
            "a derivation must carry a `meta = { … }` attrset",
        ));
    }

    findings
}

/// The always-on, hermetic structural floor for a rendered root `flake.nix`:
/// balanced delimiters + the overlay `callPackage` lines round-trip (both via
/// [`nix::flake_is_well_formed`], the SAME recovery parser the next publish
/// relies on) plus the top-level `description` / `inputs` / `outputs`
/// attributes a flake must expose. Returns one [`SchemaFinding`] per violation;
/// an empty Vec means the flake clears the floor.
///
/// Reuses `flake_is_well_formed` rather than re-deriving the balance + overlay
/// checks, so the validator and the publish-loop's recovery parser share ONE
/// definition of a well-formed flake.
pub(crate) fn validate_flake_structural(text: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();

    // Balanced delimiters (string/comment-aware) AND every overlay line
    // round-trips the recovery parser — the exact pair `flake_is_well_formed`
    // asserts. A parse/balance failure becomes a `(root)` finding.
    if let Err(e) = nix::flake_is_well_formed(text) {
        findings.push(finding("(root)", &format!("{e:#}")));
    }

    // `description` / `inputs` / `outputs` — the top-level attributes a flake
    // must expose. `nix flake check` errors without `outputs`; `description`
    // and `inputs` are the load-bearing identity/dependency attributes the
    // template always emits.
    for attr in ["description", "inputs", "outputs"] {
        if !has_attr_binding(text, attr) {
            findings.push(finding(
                attr,
                &format!("a flake must bind a top-level `{attr} = …` attribute"),
            ));
        }
    }

    findings
}

/// The gated layer: when `nix-instantiate` is on `PATH`, write the rendered
/// expression to a tempfile and run `nix-instantiate --parse <file>`. A
/// non-zero exit means a parse error in the generated Nix — parse each
/// `<file>:<line>:<col>: <message>` stderr line into a [`SchemaFinding`]. A
/// non-zero exit with no parseable line still yields a `(root)` finding (never
/// silent-pass). When `nix-instantiate` is absent, log a visible skip marker
/// and return no findings — the structural floor stands; a missing tool is
/// never an artifact defect.
fn validate_nix_syntax(
    kind: NixKind,
    nix_src: &str,
    log: &StageLogger,
) -> Result<Vec<SchemaFinding>> {
    let file_name = match kind {
        NixKind::Derivation => "default.nix",
        NixKind::Flake => "flake.nix",
    };
    super::run_external_validator(
        &super::ExternalValidator {
            publisher: "nix",
            tool: "nix-instantiate",
            flags: &["--parse"],
            files: &[(file_name, nix_src)],
            skip_message: "nix-instantiate not on PATH — relying on the structural Nix floor \
                 for syntax validation",
            empty_fallback: "nix-instantiate --parse reported the generated Nix invalid but \
                 emitted no parseable diagnostic",
        },
        parse_nix_instantiate_stderr,
        log,
    )
}

/// Parse `nix-instantiate --parse` stderr into [`SchemaFinding`]s. A parse
/// error line carries a `<file>:<line>:<col>:` position (e.g.
/// `error: syntax error … at /tmp/x/default.nix:14:3:`); the line number
/// becomes the finding field and the surrounding text its expectation. Lines
/// without a `:<digits>:<digits>:` position are ignored.
fn parse_nix_instantiate_stderr(stderr: &str) -> Vec<SchemaFinding> {
    stderr
        .lines()
        .filter_map(|line| {
            let lineno = nix_error_lineno(line)?;
            Some(finding(&format!("line {lineno}"), line.trim()))
        })
        .collect()
}

/// Extract the `<line>` number from a `:<line>:<col>:` position marker anywhere
/// in `line`, or `None` when no such marker is present. Scans for the LAST
/// such marker so a tempfile path containing colons does not mis-anchor.
fn nix_error_lineno(line: &str) -> Option<u32> {
    // Walk every `:` and test whether it begins a `:<digits>:<digits>:`
    // position; keep the last match (positions trail the file path).
    let bytes = line.as_bytes();
    let mut found: Option<u32> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b':' {
            continue;
        }
        let rest = &line[i + 1..];
        let mut parts = rest.splitn(3, ':');
        let lineno = parts.next().unwrap_or("");
        let col = parts.next().unwrap_or("");
        if !lineno.is_empty()
            && lineno.chars().all(|c| c.is_ascii_digit())
            && !col.is_empty()
            && col.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = lineno.parse::<u32>()
        {
            found = Some(n);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        CrateConfig, NixConfig, PublishConfig, ReleaseConfig, RepositoryConfig, ScmRepoConfig,
        StringOrBool,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;

    /// A `NixConfig` exercising the derivation-affecting options with values
    /// `nix-build` accepts.
    fn every_option_nix_cfg() -> NixConfig {
        NixConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("nixpkgs-overlay".to_string()),
                branch: Some("main".to_string()),
                ..Default::default()
            }),
            description: Some("A widget management tool".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            license: Some("MIT".to_string()),
            main_program: Some("widget".to_string()),
            ..Default::default()
        }
    }

    fn nix_crate(crate_name: &str, tag_template: &str, cfg: NixConfig) -> CrateConfig {
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
                nix: Some(cfg),
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

    /// Add a Linux tar.gz archive carrying the url + sha256 the derivation
    /// `src` keys off. `with_sha` toggles whether the sha256 metadata is
    /// present so the present-but-broken-artifact case can be exercised.
    fn add_linux_archive(ctx: &mut Context, crate_name: &str, version: &str, with_sha: bool) {
        let target = "x86_64-unknown-linux-gnu";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v{version}/{crate_name}-{target}.tar.gz"
            ),
        );
        if with_sha {
            meta.insert("sha256".to_string(), "a".repeat(64));
        }
        meta.insert("format".to_string(), "tar.gz".to_string());
        meta.insert("os".to_string(), "linux".to_string());
        meta.insert("arch".to_string(), "amd64".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/{crate_name}-{target}.tar.gz")),
            name: format!("{crate_name}-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }

    // -----------------------------------------------------------------
    // (a) single-crate mode — every option, zero findings + attrs land.
    // -----------------------------------------------------------------

    #[test]
    fn single_crate_every_option_validates_and_lands_in_attrs() {
        let cfg = every_option_nix_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![nix_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0", true);

        let findings = NixSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate derivation + flake must conform, got: {findings:?}"
        );

        // Parse the rendered derivation and assert each key attribute landed.
        let render = render_nix_for_validation(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        let expr = &render.expr;
        assert!(
            expr.contains("pname = \"widget\";"),
            "derivation stamps pname, got: {expr}"
        );
        assert!(
            expr.contains("version = \"1.0.0\";"),
            "derivation stamps version, got: {expr}"
        );
        assert!(
            expr.contains("fetchurl"),
            "derivation src uses fetchurl, got: {expr}"
        );
        assert!(
            expr.contains("sha256"),
            "derivation src binds sha256, got: {expr}"
        );
        assert!(
            expr.contains("license = lib.licenses.mit;"),
            "derivation meta carries the resolved license, got: {expr}"
        );
        assert!(
            expr.contains("description = \"A widget management tool\";"),
            "derivation meta carries the description, got: {expr}"
        );

        // The flake exposes the package for all systems.
        let flake = nix::generate_flake(&[FlakePackage {
            attr: render.name.clone(),
            path: format!("pkgs/{}/default.nix", render.name),
        }])
        .expect("flake renders");
        assert!(
            flake.contains("widget = pkgs.widget;"),
            "flake exposes the package, got: {flake}"
        );
        assert!(
            validate_flake_structural(&flake).is_empty(),
            "the rendered flake must clear the structural floor"
        );
    }

    // -----------------------------------------------------------------
    // (b) workspace-lockstep mode — shared version.
    // -----------------------------------------------------------------

    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = nix_crate(
            "alpha",
            "v{{ .Version }}",
            NixConfig {
                name: Some("alpha".to_string()),
                ..every_option_nix_cfg()
            },
        );
        let beta = nix_crate(
            "beta",
            "v{{ .Version }}",
            NixConfig {
                name: Some("beta".to_string()),
                ..every_option_nix_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "alpha", "1.0.0", true);
        add_linux_archive(&mut ctx, "beta", "1.0.0", true);

        let findings = NixSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace derivations + flake must conform, got: {findings:?}"
        );
    }

    // -----------------------------------------------------------------
    // (c) workspace per-crate mode — each crate its own tag/version.
    // -----------------------------------------------------------------

    #[test]
    fn workspace_per_crate_every_option_stamps_own_version() {
        let alpha = nix_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            NixConfig {
                name: Some("alpha".to_string()),
                ..every_option_nix_cfg()
            },
        );
        let beta = nix_crate(
            "beta",
            "beta-v{{ .Version }}",
            NixConfig {
                name: Some("beta".to_string()),
                ..every_option_nix_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_linux_archive(&mut ctx_a, "alpha", "2.0.0", true);
        let findings_a = NixSchemaValidator
            .validate(
                &mut ctx_a,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let render_a = render_nix_for_validation(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            render_a.expr.contains("version = \"2.0.0\";"),
            "alpha derivation stamps its own version, got: {}",
            render_a.expr
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_linux_archive(&mut ctx_b, "beta", "3.1.0", true);
        let findings_b = NixSchemaValidator
            .validate(
                &mut ctx_b,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let render_b = render_nix_for_validation(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("not skipped");
        assert!(
            render_b.expr.contains("version = \"3.1.0\";"),
            "beta derivation stamps its own version, got: {}",
            render_b.expr
        );
    }

    // -----------------------------------------------------------------
    // shard-tolerance + skip + present-but-broken artifact.
    // -----------------------------------------------------------------

    /// A single-target / sharded snapshot that built no Nix-mappable archive
    /// for a nix-configured crate must SKIP it (zero findings, no error)
    /// rather than trip the publisher's "no Linux/Darwin archive" guard.
    #[test]
    fn crate_without_matching_artifact_is_skipped_not_failed() {
        let cfg = every_option_nix_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![nix_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // No archive at all in this shard.

        let findings = NixSchemaValidator
            .validate(
                &mut ctx,
                &crate::schema_validation::test_current_version_resolver(),
            )
            .expect("validation runs without erroring on the absent archive");
        assert!(
            findings.is_empty(),
            "a crate with no Nix-mappable archive in this shard must be skipped, got: {findings:?}"
        );
    }

    /// A present-but-broken artifact — a matched archive missing its sha256 —
    /// must PROPAGATE as an error (the same defect the live publish bails on),
    /// NOT be silently skipped as a shard absence.
    #[test]
    fn present_artifact_missing_sha256_errors_not_skips() {
        let cfg = every_option_nix_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![nix_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0", false);

        let result = NixSchemaValidator.validate(
            &mut ctx,
            &crate::schema_validation::test_current_version_resolver(),
        );
        assert!(
            result.is_err(),
            "a present archive missing sha256 must error, not silently skip: {result:?}"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("sha256"),
            "the propagated error must name the missing sha256, got: {msg}"
        );
    }

    /// A crate whose `if:` renders falsy must be skipped:
    /// `render_nix_for_validation` returns `None` and the validator yields no
    /// findings for it.
    #[test]
    fn crate_with_falsy_if_is_skipped() {
        let cfg = NixConfig {
            if_condition: Some("false".to_string()),
            ..every_option_nix_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![nix_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0", true);

        let rendered =
            render_nix_for_validation(&ctx, "widget", &ctx.logger("publish")).expect("render ok");
        assert!(
            rendered.is_none(),
            "a falsy `if` must skip the crate, got a rendered derivation"
        );

        let findings = NixSchemaValidator
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

    /// `skip_upload: true` suppresses the emission entirely.
    #[test]
    fn skip_upload_suppresses_emission() {
        let cfg = NixConfig {
            skip_upload: Some(StringOrBool::Bool(true)),
            ..every_option_nix_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![nix_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_linux_archive(&mut ctx, "widget", "1.0.0", true);

        let rendered =
            render_nix_for_validation(&ctx, "widget", &ctx.logger("publish")).expect("render ok");
        assert!(
            rendered.is_none(),
            "a truthy skip_upload must suppress the emission, got a rendered derivation"
        );
    }

    // -----------------------------------------------------------------
    // biting negatives — the floor must bite, fixes must clear.
    // -----------------------------------------------------------------

    /// A conforming hand-written derivation clears the derivation floor; this
    /// anchors the biting negatives below (they all mutate THIS baseline).
    fn good_derivation() -> String {
        r#"{ lib, stdenvNoCC, fetchurl }:
stdenvNoCC.mkDerivation {
  pname = "widget";
  version = "1.0.0";

  src = fetchurl {
    url = "https://acme.example/widget.tar.gz";
    sha256 = "0000000000000000000000000000000000000000000000000000";
  };

  installPhase = ''
    mkdir -p $out/bin
    cp -vr ./widget $out/bin/widget
  '';

  meta = {
    description = "A widget";
    license = lib.licenses.mit;
  };
}
"#
        .to_string()
    }

    #[test]
    fn good_derivation_clears_the_floor() {
        assert!(
            validate_derivation_structural(&good_derivation()).is_empty(),
            "a conforming derivation must produce zero findings"
        );
    }

    #[test]
    fn derivation_unbalanced_brace_is_reported_and_fix_clears_it() {
        let broken = good_derivation().replacen("meta = {", "meta = {{", 1);
        let findings = validate_derivation_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "(root)"),
            "an unbalanced derivation must report a root finding, got: {findings:?}"
        );
        assert!(
            validate_derivation_structural(&good_derivation()).is_empty(),
            "the balanced derivation must clear it"
        );
    }

    #[test]
    fn derivation_missing_pname_is_reported_and_fix_clears_it() {
        let broken = good_derivation().replace("pname = \"widget\";", "");
        let findings = validate_derivation_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "pname"),
            "a derivation missing pname must be reported, got: {findings:?}"
        );
        assert!(
            validate_derivation_structural(&good_derivation()).is_empty(),
            "the restored derivation must clear it"
        );
    }

    #[test]
    fn derivation_missing_meta_is_reported_and_fix_clears_it() {
        // Drop the whole meta block (and its now-orphaned close brace) so the
        // delimiter balance stays intact and ONLY the missing-meta finding fires.
        let broken = good_derivation()
            .replace("  meta = {\n", "")
            .replace("    description = \"A widget\";\n", "")
            .replace("    license = lib.licenses.mit;\n", "")
            .replacen("  };\n", "", 1);
        let findings = validate_derivation_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "meta"),
            "a derivation missing meta must be reported, got: {findings:?}"
        );
        assert!(
            validate_derivation_structural(&good_derivation()).is_empty(),
            "the restored derivation must clear it"
        );
    }

    #[test]
    fn derivation_missing_sha256_is_reported_and_fix_clears_it() {
        let broken = good_derivation().replace(
            "    sha256 = \"0000000000000000000000000000000000000000000000000000\";\n",
            "",
        );
        let findings = validate_derivation_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "sha256"),
            "a derivation src missing sha256 must be reported, got: {findings:?}"
        );
    }

    /// A literal brace inside a `''…''` install body or a string must NOT trip
    /// the balance check — the string/comment-aware scan is what makes the
    /// floor lenient about valid Nix that embeds delimiter characters.
    #[test]
    fn derivation_braces_inside_strings_do_not_miscount() {
        let with_braces = r#"{ lib, stdenvNoCC, fetchurl }:
stdenvNoCC.mkDerivation {
  pname = "widget";
  version = "1.0.0";
  src = fetchurl {
    url = "https://acme.example/w.tar.gz";
    sha256 = "0000";
  };
  installPhase = ''
    echo "a brace } and a paren ) and a bracket ]"
    # a comment with { unbalanced ( delimiters [
  '';
  meta = {
    description = "desc with } brace and ${'$'}{notInterp}";
  };
}
"#;
        let findings = validate_derivation_structural(with_braces);
        assert!(
            !findings.iter().any(|f| f.field == "(root)"),
            "braces inside strings/comments must not be miscounted, got: {findings:?}"
        );
    }

    /// The Nix indented-string escapes `'''` (a literal `''`) and `''${` (a
    /// literal `${`) must NOT be mistaken for the string terminator. A
    /// user-supplied `installPhase` line containing them — with a stray `}`
    /// after, opaque because the scanner stays inside the string — must still
    /// CLEAR the floor (the too-strict false-rejection trap this guards). If
    /// the scanner ended the string early on `'''`/`''${`, the trailing `}`
    /// would land in code context and trip a bogus "(root)" imbalance.
    #[test]
    fn derivation_indented_string_escapes_do_not_miscount() {
        let with_escapes = r#"{ lib, stdenvNoCC, fetchurl }:
stdenvNoCC.mkDerivation {
  pname = "widget";
  version = "1.0.0";
  src = fetchurl {
    url = "https://acme.example/w.tar.gz";
    sha256 = "0000";
  };
  installPhase = ''
    echo "a literal quote-run ''' stays inside the string }"
    echo "a literal dollar-brace ''${PATH} also stays inside }"
  '';
  meta = {
    description = "x";
  };
}
"#;
        // Sanity: the shared scanner itself must accept it (no Err).
        assert!(
            nix::nix_delimiters_balanced(with_escapes).is_ok(),
            "the balance scanner must treat ''' and ''${{ as escapes, not terminators"
        );
        let findings = validate_derivation_structural(with_escapes);
        assert!(
            !findings.iter().any(|f| f.field == "(root)"),
            "indented-string escapes must not be miscounted, got: {findings:?}"
        );
    }

    /// A conforming flake clears the flake floor.
    fn good_flake() -> String {
        nix::generate_flake(&[FlakePackage {
            attr: "widget".to_string(),
            path: "pkgs/widget/default.nix".to_string(),
        }])
        .expect("flake renders")
    }

    #[test]
    fn good_flake_clears_the_floor() {
        assert!(
            validate_flake_structural(&good_flake()).is_empty(),
            "a conforming flake must produce zero findings"
        );
    }

    #[test]
    fn flake_unbalanced_brace_is_reported_and_fix_clears_it() {
        let broken = good_flake().replacen("inputs = {", "inputs = {{", 1);
        let findings = validate_flake_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "(root)"),
            "an unbalanced flake must report a root finding, got: {findings:?}"
        );
        assert!(
            validate_flake_structural(&good_flake()).is_empty(),
            "the balanced flake must clear it"
        );
    }

    #[test]
    fn flake_missing_outputs_is_reported_and_fix_clears_it() {
        // Rename the `outputs` binding so the attr scan misses it, while the
        // value (the function body) stays — keeping delimiters balanced so
        // ONLY the missing-outputs finding fires.
        let broken = good_flake().replacen("outputs = {", "notoutputs = {", 1);
        let findings = validate_flake_structural(&broken);
        assert!(
            findings.iter().any(|f| f.field == "outputs"),
            "a flake missing outputs must be reported, got: {findings:?}"
        );
        assert!(
            validate_flake_structural(&good_flake()).is_empty(),
            "the restored flake must clear it"
        );
    }

    // -----------------------------------------------------------------
    // gated nix-instantiate layer — skips here (tool absent).
    // -----------------------------------------------------------------

    /// When `nix-instantiate` is on `PATH`, the gated layer parses the
    /// derivation; absent (as in this sandbox), it returns no findings and the
    /// structural floor carries the assertions. This test documents the gate
    /// and the SKIP path it takes here.
    #[test]
    fn nix_instantiate_layer_is_tool_gated() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let available =
            anodizer_core::tool_detect::tool_available("nix-instantiate").unwrap_or(false);
        let findings = validate_nix_syntax(NixKind::Derivation, &good_derivation(), &log)
            .expect("syntax layer runs");
        if available {
            assert!(
                findings.is_empty(),
                "a valid derivation must parse cleanly under nix-instantiate, got: {findings:?}"
            );
        } else {
            eprintln!("SKIP: nix-instantiate not on PATH; structural floor carries the assertions");
            assert!(
                findings.is_empty(),
                "absent tool yields no findings (visible skip), got: {findings:?}"
            );
        }
    }

    /// The gated layer must BITE: a syntactically broken Nix expression fed
    /// through the real `nix-instantiate --parse` path produces a
    /// [`SchemaFinding`] (it does not silent-pass every input). Drives
    /// `validate_nix_syntax` — the same function the live validator calls — so
    /// the assertion covers the production path, not a canned-stderr unit test.
    /// Tool-gated like its sibling above: when `nix-instantiate` is absent the
    /// parse path cannot run, so it SKIPs with a visible marker rather than
    /// failing.
    #[test]
    fn nix_instantiate_layer_bites_on_malformed_input() {
        let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        if !anodizer_core::tool_detect::tool_available("nix-instantiate").unwrap_or(false) {
            eprintln!(
                "SKIP: nix-instantiate not on PATH; cannot exercise the parse-error bite path"
            );
            return;
        }

        // A `version = ;` with no value on line 4 is a syntax error
        // `nix-instantiate --parse` rejects with a `<file>:<line>:<col>:`
        // position — the exact shape the stderr parser extracts into a finding.
        let malformed = "{ lib, stdenvNoCC }:\nstdenvNoCC.mkDerivation {\n  pname = \"widget\";\n  version = ;\n}\n";
        let findings =
            validate_nix_syntax(NixKind::Derivation, malformed, &log).expect("syntax layer runs");
        assert!(
            !findings.is_empty(),
            "the gated nix-instantiate layer must report a finding for malformed Nix, got none"
        );
        // The finding must carry the parse-extracted position (`line 4`), not
        // merely the `(root)` no-silent-pass fallback: this proves the real
        // `nix-instantiate --parse` stderr drove the diagnostic.
        assert!(
            findings.iter().any(|f| f.field == "line 4"),
            "the bite must extract the parse position from nix-instantiate stderr, got: {findings:?}"
        );
    }

    // -----------------------------------------------------------------
    // stderr parser — line number extraction.
    // -----------------------------------------------------------------

    #[test]
    fn nix_instantiate_stderr_extracts_line_number() {
        let stderr = "error: syntax error, unexpected '}' at /tmp/abc/default.nix:14:3:";
        let findings = parse_nix_instantiate_stderr(stderr);
        assert!(
            findings.iter().any(|f| f.field == "line 14"),
            "the line number must be extracted, got: {findings:?}"
        );
    }

    #[test]
    fn nix_error_lineno_ignores_non_position_colons() {
        // A path with colons but no `:line:col:` position yields None.
        assert_eq!(
            nix_error_lineno("error: something happened: see docs"),
            None
        );
        assert_eq!(nix_error_lineno("at /a/b:c/default.nix:7:1:"), Some(7));
    }
}
