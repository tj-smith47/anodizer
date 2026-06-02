//! Homebrew formula + cask Ruby validation.
//!
//! Homebrew has no JSON/YAML schema: a tap file is Ruby DSL that
//! `brew audit` / `brew style` accept only when it is, at minimum, syntactically
//! valid Ruby carrying the load-bearing stanzas (`class … < Formula` / `cask "…"
//! do`, plus `url`, `sha256`, an install/artifact directive, …). anodizer
//! renders that Ruby per crate (formula, same-tap cask, standalone cask) and per
//! top-level `homebrew_casks:` entry; this validator renders the exact Ruby a
//! live publish would push — via the same render path — and checks it two ways:
//! an always-on structural floor (pure-Rust stanza scanning) and, when `ruby` is
//! on `PATH`, a real `ruby -c` syntax check. A structural defect (a missing
//! required stanza, an unbalanced template that produced broken Ruby) surfaces
//! in the snapshot/dry-run pass rather than after a tap commit ships an
//! uninstallable formula.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use super::{PublisherSchemaValidator, SchemaFinding};
use crate::homebrew::{
    CaskGenResult, crate_has_homebrew_archives, crate_has_macos_cask_artifact,
    is_homebrew_per_crate_configured, render_homebrew_cask_for_crate,
    render_homebrew_formula_for_crate, render_same_tap_cask_for_crate,
    render_top_level_homebrew_casks,
};

/// Which kind of artifact a rendered Ruby string is — drives the stanza set the
/// structural floor asserts.
#[derive(Clone, Copy)]
enum RubyKind {
    Formula,
    Cask,
}

/// Validates anodizer's rendered Homebrew formula + cask Ruby.
pub(crate) struct HomebrewSchemaValidator;

impl PublisherSchemaValidator for HomebrewSchemaValidator {
    fn publisher(&self) -> &'static str {
        "homebrew"
    }

    fn validate(&self, ctx: &Context) -> Result<Vec<SchemaFinding>> {
        let log = ctx.logger("publish");
        let mut findings = Vec::new();

        // Walk exactly the crate set the live homebrew publisher's `run`
        // iterates (honoring `--crate` selection, else every
        // homebrew-configured crate) so the validated set equals the published
        // set in all config modes.
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_homebrew_per_crate_configured,
        );
        for crate_name in &selected {
            if !is_homebrew_per_crate_configured(ctx, crate_name) {
                continue;
            }

            let hb_cfg = crate::util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.homebrew);

            // FORMULA path. A real release always builds at least one archive a
            // formula can point at, but a sharded / single-target snapshot may
            // build none for this crate. The presence probe is `bail!`-free and
            // does NOT read url/sha256: when it reports a candidate exists, the
            // render is called and ANY error propagates (`?`) — a present-but-
            // broken artifact, a missing url/sha256, is a real defect to surface,
            // not a skip. Only genuine artifact ABSENCE skips.
            if let Some(hb_cfg) = hb_cfg.as_ref() {
                if !crate_has_homebrew_archives(ctx, hb_cfg, crate_name) {
                    log.verbose(&format!(
                        "homebrew: crate '{}' produced no archive artifact in this snapshot \
                         shard; skipping formula schema validation",
                        crate_name
                    ));
                } else if let Some(rendered) =
                    render_homebrew_formula_for_crate(ctx, crate_name, &log)?
                {
                    findings.extend(validate_ruby_structural(
                        RubyKind::Formula,
                        &rendered.formula,
                    ));
                    findings.extend(validate_ruby_syntax(&rendered.formula, &log)?);
                }

                // SAME-TAP CASK path. The render needs a macOS artifact. Gate
                // the not-applicable skip on darwin-artifact PRESENCE, then call
                // the render and propagate any `Err` — a missing url/sha256 on a
                // present artifact is a real defect, not a not-applicable skip.
                if hb_cfg.cask.is_some()
                    && crate_has_macos_cask_artifact(ctx, crate_name)
                    && let Some(cask) =
                        render_same_tap_cask_for_crate(ctx, hb_cfg, crate_name, &log)?
                {
                    validate_cask_and_versioned(&mut findings, &cask, &log)?;
                }
            }

            // STANDALONE CASK path. Same darwin-presence gate over a render
            // whose `Err` propagates: a present-but-broken macOS artifact must
            // surface, only true absence skips.
            if crate_has_macos_cask_artifact(ctx, crate_name)
                && let Some(cask) = render_homebrew_cask_for_crate(ctx, crate_name, &log)?
            {
                validate_cask_and_versioned(&mut findings, &cask, &log)?;
            }
        }

        // Top-level `homebrew_casks:` (not per-crate). Empty when unconfigured
        // or no entry is applicable in this shard.
        for ruby in render_top_level_homebrew_casks(ctx, &log)? {
            findings.extend(validate_ruby_structural(RubyKind::Cask, &ruby));
            findings.extend(validate_ruby_syntax(&ruby, &log)?);
        }

        Ok(findings)
    }
}

/// The always-on, hermetic structural floor: scan the rendered Ruby for the
/// load-bearing stanzas Homebrew requires and assert each is present and
/// non-empty. Returns one [`SchemaFinding`] per violation; an empty Vec means
/// the document clears the floor. Runs with no external tools, so it holds even
/// where `ruby` is absent.
///
/// The rendered output is template-controlled, so targeted stanza scanning is
/// sufficient — this is deliberately NOT a Ruby parser. It is lenient about
/// optional stanzas (too-strict scanning would false-reject valid output).
fn validate_ruby_structural(kind: RubyKind, ruby: &str) -> Vec<SchemaFinding> {
    match kind {
        RubyKind::Formula => validate_formula_structural(ruby),
        RubyKind::Cask => validate_cask_structural(ruby),
    }
}

/// Run both layers (structural floor + `ruby -c`) over a rendered cask's
/// primary body and every versioned alt-name body, appending each finding to
/// `findings`. Shared by the same-tap and standalone cask paths.
fn validate_cask_and_versioned(
    findings: &mut Vec<SchemaFinding>,
    cask: &CaskGenResult,
    log: &StageLogger,
) -> Result<()> {
    findings.extend(validate_ruby_structural(RubyKind::Cask, &cask.content));
    findings.extend(validate_ruby_syntax(&cask.content, log)?);
    for (_alt, body) in &cask.versioned_files {
        findings.extend(validate_ruby_structural(RubyKind::Cask, body));
        findings.extend(validate_ruby_syntax(body, log)?);
    }
    Ok(())
}

fn finding(field: &str, expected: &str) -> SchemaFinding {
    SchemaFinding {
        publisher: "homebrew".to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
    }
}

/// True when `ruby` contains a `<stanza> "<value>"` line whose value is
/// non-empty — the shape Homebrew's required string stanzas (`url`, `sha256`,
/// cask `version` / `name`) take.
fn has_nonempty_string_stanza(ruby: &str, stanza: &str) -> bool {
    let prefix = format!("{stanza} \"");
    ruby.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once('"'))
            .is_some_and(|(value, _)| !value.is_empty())
    })
}

/// The required-stanza floor for a rendered FORMULA — only the stanzas
/// Homebrew HARD-rejects on (parse / `brew install` failure), never the
/// `brew audit --strict` advisories:
/// - `class <Name> < Formula` header,
/// - a non-empty `url` and `sha256` (an empty either aborts the download /
///   checksum),
/// - a `def install` block (without it `brew install` has nothing to run).
///
/// Deliberately NOT required: `license` (template-gated `{% if license %}`; a
/// licenseless formula is valid Ruby and installable — `brew audit --strict`
/// only warns), and `desc` / `homepage` (a `brew audit` advisory, not an
/// install hard-reject; the template always emits the lines so requiring them
/// non-empty would false-reject a valid metadata-light formula). Over-strict
/// scanning is itself a defect: it false-rejects manifests Homebrew accepts.
fn validate_formula_structural(ruby: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();

    // `class <CamelName> < Formula` — the formula's defining header.
    let has_class = ruby.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("class ") && t.contains("< Formula")
    });
    if !has_class {
        findings.push(finding(
            "class",
            "a formula must open with `class <Name> < Formula`",
        ));
    }

    for stanza in ["url", "sha256"] {
        if !has_nonempty_string_stanza(ruby, stanza) {
            findings.push(finding(
                stanza,
                &format!("a formula must carry a non-empty `{stanza} \"…\"` stanza"),
            ));
        }
    }

    // `def install` — without it `brew install` has nothing to run.
    if !ruby
        .lines()
        .any(|line| line.trim_start().starts_with("def install"))
    {
        findings.push(finding(
            "install",
            "a formula must define an `def install` block",
        ));
    }

    findings
}

/// Cask artifact stanzas — at least one must appear, or `brew install` has
/// nothing to place on disk.
const CASK_ARTIFACT_STANZAS: &[&str] = &["binary", "app", "pkg", "suite", "manpage", "artifact"];

/// The required-stanza floor for a rendered CASK — only the stanzas Homebrew
/// HARD-rejects on (parse / `brew install` failure), never the
/// `brew audit --strict` advisories:
/// - `cask "<token>" do` header,
/// - a non-empty `version`, `url`, `sha256` (each aborts the download /
///   checksum when empty),
/// - a non-empty `name` (the install identity),
/// - at least one artifact stanza (`binary` / `app` / …) — without one the
///   cask installs nothing.
///
/// Deliberately NOT required: `desc` and `homepage` (template-gated
/// `{% if %}`; a `brew audit` advisory, not an install hard-reject — a cask
/// without them is valid Ruby and installable). Over-strict scanning is itself
/// a defect: it false-rejects manifests Homebrew accepts.
fn validate_cask_structural(ruby: &str) -> Vec<SchemaFinding> {
    let mut findings = Vec::new();

    // `cask "<token>" do` — the cask's defining header.
    let has_header = ruby.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("cask \"") && t.trim_end().ends_with(" do")
    });
    if !has_header {
        findings.push(finding(
            "cask",
            "a cask must open with `cask \"<token>\" do`",
        ));
    }

    for stanza in ["version", "sha256", "url", "name"] {
        if !has_nonempty_string_stanza(ruby, stanza) {
            findings.push(finding(
                stanza,
                &format!("a cask must carry a non-empty `{stanza} \"…\"` stanza"),
            ));
        }
    }

    // At least one artifact stanza (`binary "…"`, `app "…"`, …) keyed on the
    // leading token, so a `binary` directive with kwargs (`binary "x", target:
    // "y"`) still counts.
    let has_artifact = ruby.lines().any(|line| {
        let t = line.trim_start();
        CASK_ARTIFACT_STANZAS
            .iter()
            .any(|stanza| t.starts_with(&format!("{stanza} \"")))
    });
    if !has_artifact {
        findings.push(finding(
            "artifact",
            "a cask must declare at least one artifact stanza (binary/app/pkg/…)",
        ));
    }

    findings
}

/// The gated layer: when `ruby` is on `PATH`, write the rendered Ruby to a
/// tempfile and run `ruby -c <file>`. A non-zero exit means a syntax error in
/// the generated Ruby — parse each `…:<line>: <message>` stderr line into a
/// [`SchemaFinding`]. A non-zero exit with no parseable line still yields a
/// `(root)` finding (never silent-pass). When `ruby` is absent, log a visible
/// skip marker and return no findings — the structural floor stands; a missing
/// tool is never a manifest defect.
fn validate_ruby_syntax(ruby: &str, log: &StageLogger) -> Result<Vec<SchemaFinding>> {
    if !anodizer_core::tool_detect::tool_available("ruby").unwrap_or(false) {
        log.verbose(
            "homebrew: ruby not on PATH; relying on the structural Ruby floor for \
             syntax validation",
        );
        return Ok(Vec::new());
    }

    let dir = tempfile::tempdir().context("homebrew: create temp dir for ruby -c validation")?;
    let ruby_path = dir.path().join("artifact.rb");
    std::fs::write(&ruby_path, ruby).context("homebrew: write rendered ruby for ruby -c")?;

    let output = std::process::Command::new("ruby")
        .arg("-c")
        .arg(&ruby_path)
        .output()
        .context("homebrew: run ruby -c")?;
    if output.status.success() {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut findings = parse_ruby_c_stderr(&stderr);
    // A non-zero exit with no parseable syntax line means ruby rejected the
    // file for a reason this parser didn't recognize (or ruby itself errored).
    // Returning an empty Vec here would silently report a failed validator as
    // PASS, so surface a fallback finding carrying the raw stderr.
    if findings.is_empty() {
        let trimmed = stderr.trim();
        let expected = if trimmed.is_empty() {
            "ruby -c reported the generated Ruby invalid but emitted no parseable diagnostic"
                .to_string()
        } else {
            trimmed.to_string()
        };
        findings.push(finding("(root)", &expected));
    }
    Ok(findings)
}

/// Parse `ruby -c` stderr into [`SchemaFinding`]s. A syntax error line has the
/// shape `<file>:<line>: <message>` (e.g.
/// `artifact.rb:14: syntax error, unexpected …`); the line number becomes the
/// finding field and the message its expectation. Lines without a `:<digits>:`
/// position (continuation / caret lines) are ignored.
fn parse_ruby_c_stderr(stderr: &str) -> Vec<SchemaFinding> {
    stderr
        .lines()
        .filter_map(|line| {
            // Split off the leading `<file>:` then a `<line>:` numeric position.
            let (_file, rest) = line.split_once(':')?;
            let (lineno, msg) = rest.split_once(':')?;
            let lineno = lineno.trim();
            if lineno.is_empty() || !lineno.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some(finding(&format!("line {lineno}"), msg.trim()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        CrateConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig, ReleaseConfig,
        RepositoryConfig, ScmRepoConfig,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;

    use super::*;

    /// A `HomebrewConfig` exercising the formula-affecting options plus an
    /// inline same-tap cask, with values Homebrew accepts.
    fn every_option_homebrew_cfg() -> HomebrewConfig {
        HomebrewConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                branch: Some("main".to_string()),
                ..Default::default()
            }),
            description: Some("A widget management tool".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            license: Some("MIT".to_string()),
            cask: Some(HomebrewCaskConfig {
                name: Some("widget".to_string()),
                description: Some("A widget management tool".to_string()),
                homepage: Some("https://acme.example/widget".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn homebrew_crate(crate_name: &str, tag_template: &str, cfg: HomebrewConfig) -> CrateConfig {
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
                homebrew: Some(cfg),
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

    /// Add a macOS (darwin) tar.gz archive carrying the url + sha256 the formula
    /// and cask both key off. The cask path requires a darwin artifact, so this
    /// also enables the same-tap cask render.
    fn add_macos_archive(ctx: &mut Context, crate_name: &str, version: &str) {
        let target = "x86_64-apple-darwin";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!(
                "https://github.com/acme/widget/releases/download/v{version}/{crate_name}-{target}.tar.gz"
            ),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("format".to_string(), "tar.gz".to_string());
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

    /// (a) Single-crate mode: one crate, every option set, formula AND same-tap
    /// cask. Both rendered Ruby files must clear the structural floor with zero
    /// findings, and each option must land in its expected stanza.
    #[test]
    fn single_crate_every_option_validates_and_lands_in_stanzas() {
        let cfg = every_option_homebrew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option single-crate formula + cask must conform, got: {findings:?}"
        );

        let log = ctx.logger("publish");
        let formula = render_homebrew_formula_for_crate(&ctx, "widget", &log)
            .expect("render ok")
            .expect("not skipped");
        let ruby = &formula.formula;
        assert!(
            ruby.lines()
                .any(|l| l.trim_start().starts_with("class Widget < Formula")),
            "formula opens with `class Widget < Formula`, got: {ruby}"
        );
        assert!(has_nonempty_string_stanza(ruby, "desc"));
        assert!(has_nonempty_string_stanza(ruby, "homepage"));
        assert!(has_nonempty_string_stanza(ruby, "url"));
        assert!(has_nonempty_string_stanza(ruby, "sha256"));
        assert!(
            ruby.contains("license \"MIT\""),
            "license stanza carries the configured SPDX id, got: {ruby}"
        );

        let hb_cfg = every_option_homebrew_cfg();
        let cask = render_same_tap_cask_for_crate(&ctx, &hb_cfg, "widget", &log)
            .expect("render ok")
            .expect("cask configured");
        let cask_ruby = &cask.content;
        assert!(
            cask_ruby
                .lines()
                .any(|l| l.trim_start().starts_with("cask \"widget\" do")),
            "cask opens with `cask \"widget\" do`, got: {cask_ruby}"
        );
        assert!(
            cask_ruby.contains("version \"1.0.0\""),
            "cask stamps its version, got: {cask_ruby}"
        );
        assert!(
            cask_ruby
                .lines()
                .any(|l| l.trim_start().starts_with("binary \"")),
            "cask declares a binary artifact, got: {cask_ruby}"
        );
    }

    /// (b) Workspace-lockstep mode: multiple crates share one global version.
    /// Each crate's formula + cask must validate independently.
    #[test]
    fn workspace_lockstep_every_option_validates() {
        let alpha = homebrew_crate(
            "alpha",
            "v{{ .Version }}",
            HomebrewConfig {
                name: Some("alpha".to_string()),
                cask: Some(HomebrewCaskConfig {
                    name: Some("alpha".to_string()),
                    description: Some("Alpha tool".to_string()),
                    homepage: Some("https://acme.example/alpha".to_string()),
                    ..Default::default()
                }),
                ..every_option_homebrew_cfg()
            },
        );
        let beta = homebrew_crate(
            "beta",
            "v{{ .Version }}",
            HomebrewConfig {
                name: Some("beta".to_string()),
                cask: Some(HomebrewCaskConfig {
                    name: Some("beta".to_string()),
                    description: Some("Beta tool".to_string()),
                    homepage: Some("https://acme.example/beta".to_string()),
                    ..Default::default()
                }),
                ..every_option_homebrew_cfg()
            },
        );
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "alpha", "1.0.0");
        add_macos_archive(&mut ctx, "beta", "1.0.0");

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "lockstep workspace formula + cask must conform, got: {findings:?}"
        );
    }

    /// (c) Workspace per-crate mode: each crate carries its own tag_template /
    /// version. The publish stage scopes the global `Version` to the per-crate
    /// value before invoking the publisher, so the validator (run per-crate via
    /// `--crate`) must conform — and stamp its own cask `version` — under each
    /// crate's own version.
    #[test]
    fn workspace_per_crate_every_option_validates_under_own_version() {
        let alpha = homebrew_crate(
            "alpha",
            "alpha-v{{ .Version }}",
            HomebrewConfig {
                name: Some("alpha".to_string()),
                cask: Some(HomebrewCaskConfig {
                    name: Some("alpha".to_string()),
                    description: Some("Alpha tool".to_string()),
                    homepage: Some("https://acme.example/alpha".to_string()),
                    ..Default::default()
                }),
                ..every_option_homebrew_cfg()
            },
        );
        let beta = homebrew_crate(
            "beta",
            "beta-v{{ .Version }}",
            HomebrewConfig {
                name: Some("beta".to_string()),
                cask: Some(HomebrewCaskConfig {
                    name: Some("beta".to_string()),
                    description: Some("Beta tool".to_string()),
                    homepage: Some("https://acme.example/beta".to_string()),
                    ..Default::default()
                }),
                ..every_option_homebrew_cfg()
            },
        );

        let mut ctx_a = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha.clone(), beta.clone()])
            .selected_crates(vec!["alpha".to_string()])
            .build();
        scope_version(&mut ctx_a, "2.0.0");
        add_macos_archive(&mut ctx_a, "alpha", "2.0.0");
        let findings_a = HomebrewSchemaValidator
            .validate(&ctx_a)
            .expect("validation runs");
        assert!(
            findings_a.is_empty(),
            "per-crate alpha@2.0.0 must conform, got: {findings_a:?}"
        );
        let cask_a = render_homebrew_cask_for_crate(&ctx_a, "alpha", &ctx_a.logger("publish"))
            .expect("render ok")
            .expect("cask configured");
        assert!(
            cask_a.content.contains("version \"2.0.0\""),
            "alpha cask stamps its own version, got: {}",
            cask_a.content
        );

        let mut ctx_b = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![alpha, beta])
            .selected_crates(vec!["beta".to_string()])
            .build();
        scope_version(&mut ctx_b, "3.1.0");
        add_macos_archive(&mut ctx_b, "beta", "3.1.0");
        let findings_b = HomebrewSchemaValidator
            .validate(&ctx_b)
            .expect("validation runs");
        assert!(
            findings_b.is_empty(),
            "per-crate beta@3.1.0 must conform, got: {findings_b:?}"
        );
        let cask_b = render_homebrew_cask_for_crate(&ctx_b, "beta", &ctx_b.logger("publish"))
            .expect("render ok")
            .expect("cask configured");
        assert!(
            cask_b.content.contains("version \"3.1.0\""),
            "beta cask stamps its own version, got: {}",
            cask_b.content
        );
    }

    /// A single-target / sharded snapshot that built no archive for a
    /// homebrew-configured crate must SKIP it (zero findings, no error) rather
    /// than trip the publisher's "no archives matched" guard. This is the exact
    /// case anodizer's own linux-only `task snapshot` hits when only a Windows
    /// archive is present.
    #[test]
    fn crate_without_matching_artifact_is_skipped_not_failed() {
        let cfg = every_option_homebrew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // No archive artifact at all in this shard.

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs without erroring on the absent archive");
        assert!(
            findings.is_empty(),
            "a crate with no archive in this shard must be skipped, got: {findings:?}"
        );
    }

    /// A crate whose `if:` renders falsy must be skipped:
    /// `render_homebrew_formula_for_crate` returns `None` and the validator
    /// yields no findings for it.
    #[test]
    fn crate_with_falsy_if_is_skipped() {
        let cfg = HomebrewConfig {
            if_condition: Some("false".to_string()),
            ..every_option_homebrew_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");

        let rendered = render_homebrew_formula_for_crate(&ctx, "widget", &ctx.logger("publish"))
            .expect("render ok");
        assert!(
            rendered.is_none(),
            "a falsy `if` must skip the crate, got a rendered formula"
        );

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a skipped crate yields no findings, got: {findings:?}"
        );
    }

    /// `skip_upload: true` on the cask suppresses the same-tap cask render
    /// (the formula still renders).
    #[test]
    fn cask_skip_upload_suppresses_same_tap_cask() {
        let hb_cfg = HomebrewConfig {
            cask: Some(HomebrewCaskConfig {
                name: Some("widget".to_string()),
                skip_upload: Some(anodizer_core::config::StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..every_option_homebrew_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate(
                "widget",
                "v{{ .Version }}",
                hb_cfg.clone(),
            )])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");

        let cask = render_same_tap_cask_for_crate(&ctx, &hb_cfg, "widget", &ctx.logger("publish"))
            .expect("render ok");
        assert!(
            cask.is_none(),
            "a truthy cask skip_upload must suppress the cask, got a rendered cask"
        );
    }

    /// The structural floor must BITE for a FORMULA: a Ruby body missing the
    /// `sha256` stanza is reported with a named finding. The corrected body
    /// produces zero findings, proving the test bites rather than always-failing.
    #[test]
    fn formula_missing_sha256_is_reported_and_fix_clears_it() {
        let broken = r#"class Widget < Formula
  desc "A widget"
  homepage "https://acme.example"
  license "MIT"
  version "1.0.0"
  url "https://acme.example/widget.tar.gz"

  def install
    bin.install "widget"
  end
end
"#;
        let findings = validate_formula_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "sha256"),
            "a formula missing sha256 must be reported, got: {findings:?}"
        );

        let fixed = broken.replace(
            "  url \"https://acme.example/widget.tar.gz\"\n",
            "  url \"https://acme.example/widget.tar.gz\"\n  sha256 \"abc123\"\n",
        );
        let fixed_findings = validate_formula_structural(&fixed);
        assert!(
            fixed_findings.is_empty(),
            "the corrected formula must produce zero findings, got: {fixed_findings:?}"
        );
    }

    /// The structural floor must BITE for a CASK: a Ruby body missing every
    /// artifact stanza is reported. Adding a `binary` stanza clears it.
    #[test]
    fn cask_missing_artifact_is_reported_and_fix_clears_it() {
        let broken = r#"cask "widget" do
  version "1.0.0"
  sha256 "abc123"
  url "https://acme.example/widget.zip"
  name "Widget"
  desc "A widget"
  homepage "https://acme.example"
end
"#;
        let findings = validate_cask_structural(broken);
        assert!(
            findings.iter().any(|f| f.field == "artifact"),
            "a cask with no artifact stanza must be reported, got: {findings:?}"
        );

        let fixed = broken.replace(
            "  homepage \"https://acme.example\"\n",
            "  homepage \"https://acme.example\"\n  binary \"widget\"\n",
        );
        let fixed_findings = validate_cask_structural(&fixed);
        assert!(
            fixed_findings.is_empty(),
            "the corrected cask must produce zero findings, got: {fixed_findings:?}"
        );
    }

    /// The `ruby -c` stderr parser maps a `<file>:<line>: <message>` syntax
    /// line to a finding whose field is the line number and whose expectation
    /// is the message. Holds even where ruby itself is absent.
    #[test]
    fn ruby_c_stderr_parses_into_findings() {
        let stderr = "artifact.rb:14: syntax error, unexpected end-of-input, expecting `end'\n";
        let findings = parse_ruby_c_stderr(stderr);
        assert_eq!(findings.len(), 1, "one syntax line, got: {findings:?}");
        assert_eq!(findings[0].publisher, "homebrew");
        assert_eq!(findings[0].field, "line 14");
        assert!(
            findings[0].expected.contains("syntax error"),
            "expectation carries the diagnostic, got: {}",
            findings[0].expected
        );
    }

    /// The `ruby -c` layer must accept the every-option formula AND cask: render
    /// them and run them through the REAL `ruby -c`, asserting zero findings.
    /// Skipped (with a visible marker) when `ruby` is not on `PATH`.
    #[test]
    fn ruby_c_accepts_every_option_formula_and_cask() {
        let cfg = every_option_homebrew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");
        let log = ctx.logger("publish");

        if !anodizer_core::tool_detect::tool_available("ruby").unwrap_or(false) {
            log.status("SKIP ruby_c_accepts_every_option_formula_and_cask: ruby not on PATH (syntax layer unexercised)");
            return;
        }

        let formula = render_homebrew_formula_for_crate(&ctx, "widget", &log)
            .expect("render ok")
            .expect("not skipped");
        let formula_findings = validate_ruby_syntax(&formula.formula, &log).expect("ruby -c runs");
        assert!(
            formula_findings.is_empty(),
            "the every-option formula must pass ruby -c, got: {formula_findings:?}"
        );

        let hb_cfg = every_option_homebrew_cfg();
        let cask = render_same_tap_cask_for_crate(&ctx, &hb_cfg, "widget", &log)
            .expect("render ok")
            .expect("cask configured");
        let cask_findings = validate_ruby_syntax(&cask.content, &log).expect("ruby -c runs");
        assert!(
            cask_findings.is_empty(),
            "the every-option cask must pass ruby -c, got: {cask_findings:?}"
        );
    }

    /// The `ruby -c` layer must BITE: a syntactically-broken Ruby (an unclosed
    /// block) must produce a finding from the real `ruby -c`, and a corrected
    /// body must produce none. Skipped (with a visible marker) when `ruby` is
    /// not on `PATH`.
    #[test]
    fn ruby_c_rejects_broken_ruby() {
        let ctx = TestContextBuilder::new().snapshot(true).build();
        let log = ctx.logger("publish");

        if !anodizer_core::tool_detect::tool_available("ruby").unwrap_or(false) {
            log.status(
                "SKIP ruby_c_rejects_broken_ruby: ruby not on PATH (syntax layer unexercised)",
            );
            return;
        }

        // An unclosed `do` block — valid stanzas, invalid Ruby syntax.
        let broken = "cask \"widget\" do\n  version \"1.0.0\"\n";
        let findings = validate_ruby_syntax(broken, &log).expect("ruby -c runs");
        assert!(
            !findings.is_empty(),
            "broken Ruby must be rejected by ruby -c, got no findings"
        );

        let fixed = "cask \"widget\" do\n  version \"1.0.0\"\nend\n";
        let fixed_findings = validate_ruby_syntax(fixed, &log).expect("ruby -c runs");
        assert!(
            fixed_findings.is_empty(),
            "the corrected Ruby must pass ruby -c, got: {fixed_findings:?}"
        );
    }

    /// B3: the structural floor must NOT false-reject a VALID formula that omits
    /// the optional `license` stanza (template-gated `{% if license %}`; a
    /// licenseless formula is valid Ruby and installable). It must clear the
    /// floor with zero findings.
    #[test]
    fn formula_without_license_clears_the_floor() {
        let no_license = r#"class Widget < Formula
  desc "A widget"
  homepage "https://acme.example"
  version "1.0.0"
  url "https://acme.example/widget.tar.gz"
  sha256 "abc123"

  def install
    bin.install "widget"
  end
end
"#;
        let findings = validate_formula_structural(no_license);
        assert!(
            findings.is_empty(),
            "a licenseless formula is valid and must clear the floor, got: {findings:?}"
        );
    }

    /// B3: the structural floor must NOT false-reject a VALID cask that omits
    /// the optional `desc` / `homepage` stanzas (template-gated `{% if %}`; a
    /// cask without them is valid Ruby and installable). It must clear the floor
    /// with zero findings.
    #[test]
    fn cask_without_desc_or_homepage_clears_the_floor() {
        let no_desc_homepage = r#"cask "widget" do
  version "1.0.0"
  sha256 "abc123"
  url "https://acme.example/widget.zip"
  name "Widget"

  binary "widget"
end
"#;
        let findings = validate_cask_structural(no_desc_homepage);
        assert!(
            findings.is_empty(),
            "a cask without desc/homepage is valid and must clear the floor, got: {findings:?}"
        );
    }

    /// A top-level `homebrew_casks:` entry every-option config (its own
    /// repository + macOS artifact) must validate with zero findings, and the
    /// rendered cask must stamp `version` and a `binary` artifact stanza.
    #[test]
    fn top_level_cask_every_option_validates_and_lands_in_stanzas() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                ..Default::default()
            }),
            description: Some("A widget management tool".to_string()),
            homepage: Some("https://acme.example/widget".to_string()),
            ..Default::default()
        }]);

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "every-option top-level cask must conform, got: {findings:?}"
        );

        let rendered =
            render_top_level_homebrew_casks(&ctx, &ctx.logger("publish")).expect("render ok");
        assert_eq!(rendered.len(), 1, "one top-level cask rendered");
        let cask_ruby = &rendered[0];
        assert!(
            cask_ruby
                .lines()
                .any(|l| l.trim_start().starts_with("cask \"widget\" do")),
            "cask opens with `cask \"widget\" do`, got: {cask_ruby}"
        );
        assert!(
            cask_ruby.contains("version \"1.0.0\""),
            "top-level cask stamps its version, got: {cask_ruby}"
        );
        assert!(
            cask_ruby
                .lines()
                .any(|l| l.trim_start().starts_with("binary \"")),
            "top-level cask declares a binary artifact, got: {cask_ruby}"
        );
    }

    /// A top-level cask entry whose `skip_upload` is truthy (or whose `if:`
    /// renders falsy) must render to nothing — the validator yields no findings
    /// and `render_top_level_homebrew_casks` returns an empty Vec.
    #[test]
    fn top_level_cask_skip_renders_nothing() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .build();
        scope_version(&mut ctx, "1.0.0");
        add_macos_archive(&mut ctx, "widget", "1.0.0");
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                ..Default::default()
            }),
            skip_upload: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        }]);

        let rendered =
            render_top_level_homebrew_casks(&ctx, &ctx.logger("publish")).expect("render ok");
        assert!(
            rendered.is_empty(),
            "a skipped top-level cask must render nothing, got: {rendered:?}"
        );

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs");
        assert!(
            findings.is_empty(),
            "a skipped top-level cask yields no findings, got: {findings:?}"
        );

        // A falsy `if:` must likewise render nothing.
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                ..Default::default()
            }),
            if_condition: Some("false".to_string()),
            ..Default::default()
        }]);
        let rendered_if =
            render_top_level_homebrew_casks(&ctx, &ctx.logger("publish")).expect("render ok");
        assert!(
            rendered_if.is_empty(),
            "a falsy `if:` top-level cask must render nothing, got: {rendered_if:?}"
        );
    }

    /// A top-level cask configured in a shard that built NO darwin artifact must
    /// be NOT-APPLICABLE: it renders nothing (zero findings, no error) rather
    /// than trip the publisher's "no macOS artifact" guard.
    #[test]
    fn top_level_cask_without_darwin_artifact_is_not_applicable() {
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .project_name("widget")
            .build();
        scope_version(&mut ctx, "1.0.0");
        // Only a linux archive — no darwin artifact in this shard.
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            "https://github.com/acme/widget/releases/download/v1.0.0/widget-x86_64-unknown-linux-gnu.tar.gz"
                .to_string(),
        );
        meta.insert("sha256".to_string(), "a".repeat(64));
        meta.insert("format".to_string(), "tar.gz".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/dist/widget-x86_64-unknown-linux-gnu.tar.gz"),
            name: "widget-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("widget".to_string()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".to_string()),
                name: Some("homebrew-tap".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]);

        let rendered = render_top_level_homebrew_casks(&ctx, &ctx.logger("publish"))
            .expect("render must not error on the absent darwin artifact");
        assert!(
            rendered.is_empty(),
            "a top-level cask with no darwin artifact must render nothing, got: {rendered:?}"
        );

        let findings = HomebrewSchemaValidator
            .validate(&ctx)
            .expect("validation runs without erroring");
        assert!(
            findings.is_empty(),
            "a not-applicable top-level cask yields no findings, got: {findings:?}"
        );
    }

    /// B1/B2: a PRESENT formula archive whose metadata is broken (missing
    /// sha256) is a REAL defect the live publish would refuse to push — the
    /// validator must SURFACE it (an `Err` from the render propagates), NOT
    /// collapse it to a not-applicable skip. Guards the regression where every
    /// render error was pattern-matched away as a shard skip.
    #[test]
    fn present_formula_archive_missing_sha256_surfaces_not_skips() {
        let cfg = every_option_homebrew_cfg();
        let mut ctx = TestContextBuilder::new()
            .snapshot(true)
            .crates(vec![homebrew_crate("widget", "v{{ .Version }}", cfg)])
            .build();
        scope_version(&mut ctx, "1.0.0");
        // A darwin archive that is PRESENT (clears the presence probe) but
        // carries no sha256 — the formula render bails on it.
        let target = "x86_64-apple-darwin";
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            format!("https://acme.example/v1.0.0/widget-{target}.tar.gz"),
        );
        meta.insert("format".to_string(), "tar.gz".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/dist/widget-{target}.tar.gz")),
            name: format!("widget-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: "widget".to_string(),
            metadata: meta,
            size: None,
        });

        let result = HomebrewSchemaValidator.validate(&ctx);
        let err = result.expect_err(
            "a present-but-broken archive (missing sha256) must surface as an error, \
             not a silent skip",
        );
        assert!(
            format!("{err:#}").contains("sha256"),
            "the surfaced error must name the missing sha256, got: {err:#}"
        );
    }
}
