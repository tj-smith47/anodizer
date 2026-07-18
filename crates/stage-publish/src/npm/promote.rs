//! NPM dist-tag promotion — the [`Promotable`] implementation for npm.
//!
//! Moves an already-published npm version onto a more stable dist-tag with
//! `npm dist-tag add <pkg>@<version> <tag>` — a pointer move, no republish and
//! no 72-hour irreversibility. A promotion re-tags **every** package the npm
//! publisher published, not just the user-facing metapackage: in
//! `optional-deps` mode the metapackage's `optionalDependencies` pin each
//! per-platform package at an exact version, so leaving the platform packages
//! on the old dist-tag while the metapackage moves would desync the family.
//!
//! The version and package set are resolved from the [`PromoteSelector`]:
//! * [`PromoteSelector::FromRun`] → the exact `package@version` set the prior
//!   run recorded in its npm [`PublishEvidence`] — authoritative, no registry
//!   round-trip.
//! * [`PromoteSelector::Version`] → that version; the family is read from the
//!   metapackage's published `optionalDependencies`.
//! * [`PromoteSelector::Newest`] → the version currently under the `from`
//!   dist-tag (`npm dist-tag ls`), family as above.
//!
//! [`PublishEvidence`]: anodizer_core::PublishEvidence

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anodizer_core::config::{NpmConfig, NpmMode};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::promote::{
    Promotable, PromoteOutcome, PromoteRequest, PromoteSelector, PromoteSkipReason,
    is_canonical_pretrack, partial_promotion_error,
};
use anodizer_core::run::run_checked;
use anodizer_core::{PublishEvidenceExtra, PublishReport};
use anyhow::{Context as _, Result, bail};
use tempfile::TempDir;

use super::manifest::{resolve_name, resolve_registry};
use super::optional_deps::resolve_metapackage;
use super::publish::{NpmAuth, resolve_token, write_npmrc};

/// The dist-tag npm's canonical `stable` track maps to.
const STABLE_DIST_TAG: &str = "latest";
/// Fallback pre-stable dist-tag when the project configures none.
const DEFAULT_PRE_DIST_TAG: &str = "next";

/// The npm promotion capability.
///
/// Carries the project's pre-stable dist-tag so
/// [`resolve_track`](Promotable::resolve_track) can map the publisher-neutral
/// `prerelease`/`candidate`/`beta` aliases onto whatever dist-tag this project
/// publishes prereleases under (its configured `npms[].tag`, else `next`).
pub struct NpmPromoter {
    pre_tag: String,
}

impl NpmPromoter {
    /// Build a promoter whose pre-stable aliases resolve to `pre_tag` (e.g. the
    /// project's configured `npms[].tag`).
    pub fn new(pre_tag: impl Into<String>) -> Self {
        Self {
            pre_tag: pre_tag.into(),
        }
    }
}

impl Default for NpmPromoter {
    fn default() -> Self {
        Self::new(DEFAULT_PRE_DIST_TAG)
    }
}

impl Promotable for NpmPromoter {
    fn name(&self) -> &str {
        "npm"
    }

    /// `stable` maps to npm's conventional `latest` dist-tag; every canonical
    /// pre-stable alias (`prerelease`/`candidate`/`beta`/`edge`) maps to the
    /// project's single pre-stable dist-tag. Anything else — including a raw
    /// dist-tag the operator typed directly (`next`, `canary`, …) — passes
    /// through verbatim.
    fn resolve_track(&self, canonical: &str) -> String {
        if canonical == "stable" {
            STABLE_DIST_TAG.to_string()
        } else if is_canonical_pretrack(canonical) {
            self.pre_tag.clone()
        } else {
            canonical.to_string()
        }
    }

    fn promote(&self, req: &PromoteRequest) -> Result<PromoteOutcome> {
        let log = req.ctx.logger("npm-promote");

        let entries: Vec<&NpmConfig> = req.ctx.config.npms.iter().flatten().collect();
        if entries.is_empty() {
            bail!("no npm config; `anodizer promote --publishers npm` needs an `npms:` block");
        }

        // The `from` shown in the folded outcome names the source the selector
        // actually targets (`--version`/`--from-run`), not the canonical track.
        let from_label = req.selector.source_label(&req.from);

        // Dry-run: name the plan and spawn nothing. Concrete version / family
        // resolution needs a registry round-trip, so dry-run names the selector
        // rather than the exact packages, mirroring the snapcraft promoter.
        if req.dry_run {
            log.status(&format!(
                "(dry-run) would promote npm {} {}→{}",
                req.selector.describe(),
                req.from,
                req.to
            ));
            return Ok(PromoteOutcome::dry_run(
                self.name(),
                from_label,
                &req.to,
                None,
            ));
        }

        // One `.npmrc` per (registry, token) pair — distinct tokens on the same
        // registry must not share credentials (see ReTagger::npmrc).
        let cfg_dir = TempDir::new().context("npm: create promote scratch dir")?;
        let mut retag = ReTagger::new(req, &log, cfg_dir.path());

        // Best-effort across the family: one package's (or one `npms[]` entry's)
        // failure does not abort the rest — every target is attempted and the
        // applied-vs-failed split is surfaced at the end.
        match req.selector {
            PromoteSelector::FromRun { report, .. } => {
                retag.retag_recorded(report);
            }
            _ => {
                for cfg in &entries {
                    let family = family_label(req.ctx, cfg);
                    if let Err(err) = retag.retag_config(cfg) {
                        retag.failed.push((family, format!("{err:#}")));
                    }
                }
            }
        }

        if !retag.failed.is_empty() {
            bail!("{}", partial_promotion_error(&retag.applied, &retag.failed));
        }

        if retag.applied.is_empty() {
            log.status(&format!(
                "no npm package found for {} in {} — nothing to promote",
                req.selector.describe(),
                req.from
            ));
            return Ok(PromoteOutcome::skipped(
                self.name(),
                from_label,
                &req.to,
                PromoteSkipReason::NothingToPromote,
            ));
        }

        let count = retag.applied.len();
        log.status(&format!(
            "promoted {count} npm package(s) {}→{}",
            req.from, req.to
        ));
        Ok(PromoteOutcome::promoted(
            self.name(),
            from_label,
            &req.to,
            format!("{count} package(s)"),
        ))
    }
}

/// Operator-facing label for one `npms[]` entry's family, used when the family's
/// setup (registry / token / version resolution) fails before any per-package
/// re-tag runs.
fn family_label(ctx: &Context, cfg: &NpmConfig) -> String {
    let crate_name = fallback_crate_name(ctx);
    match cfg.mode {
        NpmMode::OptionalDeps => resolve_metapackage(cfg, &crate_name).to_string(),
        NpmMode::Postinstall => resolve_name(cfg, &crate_name).to_string(),
    }
}

/// Drives the per-package `npm dist-tag add`, caching one authenticated
/// `.npmrc` per registry so a multi-package family shares credentials — but the
/// token that writes each registry's `.npmrc` comes from THAT registry's
/// config/target, so a multi-registry family authenticates every package with
/// its own registry's token.
struct ReTagger<'a> {
    req: &'a PromoteRequest<'a>,
    log: &'a StageLogger,
    cfg_dir: &'a Path,
    /// (registry endpoint, token) → written `.npmrc` path. Keyed on the token
    /// too so two targets on the SAME registry with DIFFERENT credentials each
    /// get their own authenticated `.npmrc` (a registry-only key would reuse the
    /// first target's token for the second).
    npmrcs: BTreeMap<(String, String), PathBuf>,
    /// Package labels (`pkg@version`) successfully re-tagged.
    applied: Vec<String>,
    /// Per-target failures (`label`, rendered cause) — folded into the
    /// best-effort partial-failure report.
    failed: Vec<(String, String)>,
}

impl<'a> ReTagger<'a> {
    fn new(req: &'a PromoteRequest<'a>, log: &'a StageLogger, cfg_dir: &'a Path) -> Self {
        Self {
            req,
            log,
            cfg_dir,
            npmrcs: BTreeMap::new(),
            applied: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// Re-tag exactly the `package@version` set a prior run recorded — the
    /// authoritative family, including every per-platform package. Each target
    /// carries its OWN registry + `token_env_var`, so a multi-registry family
    /// authenticates each package against the right registry. Best-effort: a
    /// per-target setup or re-tag failure is recorded, not aborted.
    fn retag_recorded(&mut self, report: &PublishReport) {
        for (package, version, registry, token_env_var) in recorded_npm_targets(report) {
            let label = format!("{package}@{version}");
            match self.env_token(&token_env_var) {
                Ok(token) => match self.npmrc(&registry, &token) {
                    Ok(npmrc) => self.dist_tag_add(&package, &version, &registry, &npmrc),
                    Err(err) => self.failed.push((label, format!("{err:#}"))),
                },
                Err(err) => self.failed.push((label, format!("{err:#}"))),
            }
        }
    }

    /// Resolve one `npms[]` entry's family and re-tag it. The token comes from
    /// THIS `cfg` (its own registry), so distinct `npms[]` entries targeting
    /// different registries each authenticate correctly.
    fn retag_config(&mut self, cfg: &NpmConfig) -> Result<()> {
        let registry = resolve_registry(self.req.ctx, cfg)?;
        let crate_name = fallback_crate_name(self.req.ctx);
        let metapackage = match cfg.mode {
            NpmMode::OptionalDeps => resolve_metapackage(cfg, &crate_name).to_string(),
            NpmMode::Postinstall => resolve_name(cfg, &crate_name).to_string(),
        };

        let token = resolve_token(self.req.ctx, cfg)?;
        let npmrc = self.npmrc(&registry, &token)?;

        let Some(version) = self.resolve_version(&metapackage, &registry, &npmrc)? else {
            return Ok(()); // nothing under the from-tag for this entry
        };

        // Re-tag the metapackage first, then each per-platform package the
        // metapackage depends on (optional-deps only; postinstall is a single
        // package). Per-package re-tags are best-effort (recorded, not aborted);
        // the platform-package *listing* is family setup and still hard-fails.
        self.dist_tag_add(&metapackage, &version, &registry, &npmrc);
        if matches!(cfg.mode, NpmMode::OptionalDeps) {
            for (pkg, ver) in self.platform_packages(&metapackage, &version, &registry, &npmrc)? {
                self.dist_tag_add(&pkg, &ver, &registry, &npmrc);
            }
        }
        Ok(())
    }

    /// Resolve an auth token from the env var a recorded target named. An
    /// unset/empty var is an actionable per-target failure (it names the var).
    fn env_token(&self, token_env_var: &str) -> Result<String> {
        self.req
            .ctx
            .env_source()
            .var(token_env_var)
            .filter(|v| !v.is_empty())
            .with_context(|| {
                format!(
                    "npm: env var ${token_env_var} (the token for this package's \
                     registry) is unset or empty — export it to re-tag this package"
                )
            })
    }

    /// Resolve the version to promote for `Version` / `Newest` selectors.
    /// `Ok(None)` means the `from` dist-tag names no version (nothing to
    /// promote); `FromRun` never reaches here.
    fn resolve_version(
        &self,
        metapackage: &str,
        registry: &str,
        npmrc: &Path,
    ) -> Result<Option<String>> {
        match self.req.selector {
            PromoteSelector::Version(v) => Ok(Some(v.clone())),
            PromoteSelector::Newest => {
                let args = npm_dist_tag_ls_command(metapackage, npmrc, registry);
                let out = self.capture(&args, "npm dist-tag ls")?;
                Ok(parse_dist_tag_version(&out, &self.req.from))
            }
            PromoteSelector::FromRun { .. } => unreachable!("FromRun handled by from_recorded"),
        }
    }

    /// Read the metapackage's published `optionalDependencies` — the exact
    /// per-platform package family pinned by that version.
    fn platform_packages(
        &self,
        metapackage: &str,
        version: &str,
        registry: &str,
        npmrc: &Path,
    ) -> Result<Vec<(String, String)>> {
        let args = npm_view_optional_deps_command(metapackage, version, npmrc, registry);
        let out = self.capture(&args, "npm view optionalDependencies")?;
        parse_optional_deps(&out)
    }

    /// Return the `.npmrc` for `(registry, token)`, writing it once on first
    /// use. Keyed on the token as well as the registry so a second target on the
    /// same registry with a different token authenticates with ITS credential.
    fn npmrc(&mut self, registry: &str, token: &str) -> Result<PathBuf> {
        let key = (registry.to_string(), token.to_string());
        if let Some(p) = self.npmrcs.get(&key) {
            return Ok(p.clone());
        }
        // Per-entry subdir so distinct (registry, token) pairs do not collide on
        // the single `.npmrc` filename `write_npmrc` fixes.
        let dir = self.cfg_dir.join(format!("r{}", self.npmrcs.len()));
        std::fs::create_dir_all(&dir).context("npm: create per-registry npmrc dir")?;
        let path = write_npmrc(&dir, registry, &NpmAuth::Token(token.to_string()), None)?;
        self.npmrcs.insert(key, path.clone());
        Ok(path)
    }

    /// Re-tag one package best-effort: success is recorded in `applied`, failure
    /// in `failed` (never aborting the rest of the family).
    fn dist_tag_add(&mut self, package: &str, version: &str, registry: &str, npmrc: &Path) {
        let label = format!("{package}@{version}");
        let args = npm_dist_tag_add_command(package, version, &self.req.to, npmrc, registry);
        self.log.verbose(&format!("running {}", args.join(" ")));
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]);
        match run_checked(&mut cmd, self.log, "npm dist-tag add")
            .with_context(|| format!("failed to re-tag {label} to {}", self.req.to))
        {
            Ok(_) => self.applied.push(label),
            Err(err) => self.failed.push((label, format!("{err:#}"))),
        }
    }

    fn capture(&self, args: &[String], label: &'static str) -> Result<String> {
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]);
        let out = run_checked(&mut cmd, self.log, label)?;
        Ok(self.log.redact(&String::from_utf8_lossy(&out.stdout)))
    }
}

/// The package-name fallback for the top-level `npms:` block, mirroring the npm
/// publisher: the first crate name, else the project name.
fn fallback_crate_name(ctx: &Context) -> String {
    ctx.config
        .primary_crate_name()
        .map(str::to_string)
        .unwrap_or_else(|| ctx.config.project_name.clone())
}

/// Pull every recorded `(package, version, registry, token_env_var)` from a
/// prior run's npm [`PublishEvidence`] — the authoritative family a promotion
/// re-tags. `token_env_var` is retained per-target so a multi-registry family
/// authenticates each package against its own registry's token.
fn recorded_npm_targets(report: &PublishReport) -> Vec<(String, String, String, String)> {
    report
        .results
        .iter()
        .filter(|r| r.name == "npm")
        .filter_map(|r| r.evidence.as_ref())
        .filter_map(|e| match &e.extra {
            PublishEvidenceExtra::Npm(n) => Some(&n.npm_targets),
            _ => None,
        })
        .flatten()
        .map(|t| {
            (
                t.package.clone(),
                t.version.clone(),
                t.registry.clone(),
                t.token_env_var.clone(),
            )
        })
        .collect()
}

/// `npm dist-tag add <pkg>@<version> <tag> --userconfig <npmrc> --registry <url>`.
fn npm_dist_tag_add_command(
    package: &str,
    version: &str,
    tag: &str,
    npmrc: &Path,
    registry: &str,
) -> Vec<String> {
    vec![
        "npm".to_string(),
        "dist-tag".to_string(),
        "add".to_string(),
        format!("{package}@{version}"),
        tag.to_string(),
        "--userconfig".to_string(),
        npmrc.display().to_string(),
        "--registry".to_string(),
        registry.to_string(),
    ]
}

/// `npm dist-tag ls <pkg> --userconfig <npmrc> --registry <url>`.
fn npm_dist_tag_ls_command(package: &str, npmrc: &Path, registry: &str) -> Vec<String> {
    vec![
        "npm".to_string(),
        "dist-tag".to_string(),
        "ls".to_string(),
        package.to_string(),
        "--userconfig".to_string(),
        npmrc.display().to_string(),
        "--registry".to_string(),
        registry.to_string(),
    ]
}

/// `npm view <pkg>@<version> optionalDependencies --json --userconfig <npmrc> --registry <url>`.
fn npm_view_optional_deps_command(
    package: &str,
    version: &str,
    npmrc: &Path,
    registry: &str,
) -> Vec<String> {
    vec![
        "npm".to_string(),
        "view".to_string(),
        format!("{package}@{version}"),
        "optionalDependencies".to_string(),
        "--json".to_string(),
        "--userconfig".to_string(),
        npmrc.display().to_string(),
        "--registry".to_string(),
        registry.to_string(),
    ]
}

/// Parse `npm dist-tag ls` output (`<tag>: <version>` lines) and return the
/// version currently under `tag`. `None` when the tag is absent.
fn parse_dist_tag_version(output: &str, tag: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (t, v) = line.split_once(':')?;
        (t.trim() == tag).then(|| v.trim().to_string())
    })
}

/// Parse `npm view <pkg> optionalDependencies --json` output into
/// `(name, version)` pairs. npm prints an empty string (or nothing) when the
/// field is absent, which yields an empty family.
fn parse_optional_deps(output: &str) -> Result<Vec<(String, String)>> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "undefined" {
        return Ok(Vec::new());
    }
    let map: BTreeMap<String, String> = serde_json::from_str(trimmed)
        .context("npm: parse optionalDependencies JSON from `npm view`")?;
    Ok(map.into_iter().collect())
}

/// Preflight for npm promotion: the `npm` CLI must be on `PATH` and an
/// `NPM_TOKEN` must be resolvable. Unlike a publish, a dist-tag move cannot use
/// OIDC Trusted-Publishing credentials (those are publish-only and cannot
/// mutate tags), so a long-lived token is required. Called by the verb only
/// when npm is among the selected publishers.
pub fn preflight(ctx: &Context) -> Result<()> {
    if !anodizer_core::tool_detect::on_path("npm") {
        bail!(
            "`npm` not found on PATH — npm promotion runs `npm dist-tag add`; \
             install Node.js/npm or deselect it with --publishers"
        );
    }
    let has_token = ctx
        .config
        .npms
        .iter()
        .flatten()
        .any(|cfg| resolve_token(ctx, cfg).is_ok());
    if !has_token {
        bail!(
            "no npm auth token resolved — npm promotion re-tags a published \
             package and needs a long-lived `NPM_TOKEN` (OIDC publish credentials \
             cannot move dist-tags)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_track_maps_canonical_else_identity() {
        let p = NpmPromoter::new("next");
        assert_eq!(p.resolve_track("stable"), "latest");
        assert_eq!(p.resolve_track("prerelease"), "next");
        assert_eq!(p.resolve_track("candidate"), "next");
        assert_eq!(p.resolve_track("beta"), "next");
        // `edge` is a canonical pre-stable alias → the project's pre-tag.
        assert_eq!(p.resolve_track("edge"), "next");
        // A raw dist-tag passes through verbatim.
        assert_eq!(p.resolve_track("canary"), "canary");
    }

    #[test]
    fn resolve_track_honors_configured_pre_tag() {
        let p = NpmPromoter::new("rc");
        assert_eq!(p.resolve_track("prerelease"), "rc");
        assert_eq!(p.resolve_track("stable"), "latest");
    }

    #[test]
    fn default_pre_tag_is_next() {
        assert_eq!(NpmPromoter::default().resolve_track("prerelease"), "next");
    }

    #[test]
    fn dist_tag_add_command_is_positional() {
        let args = npm_dist_tag_add_command(
            "@scope/app",
            "1.2.3",
            "latest",
            Path::new("/tmp/.npmrc"),
            "https://registry.npmjs.org",
        );
        assert_eq!(
            args,
            vec![
                "npm",
                "dist-tag",
                "add",
                "@scope/app@1.2.3",
                "latest",
                "--userconfig",
                "/tmp/.npmrc",
                "--registry",
                "https://registry.npmjs.org",
            ]
        );
    }

    #[test]
    fn dist_tag_ls_and_view_commands() {
        let ls = npm_dist_tag_ls_command("app", Path::new("/x/.npmrc"), "https://r");
        assert_eq!(ls[0..4], ["npm", "dist-tag", "ls", "app"]);
        let view =
            npm_view_optional_deps_command("app", "1.0.0", Path::new("/x/.npmrc"), "https://r");
        assert_eq!(
            view[0..5],
            ["npm", "view", "app@1.0.0", "optionalDependencies", "--json"]
        );
    }

    #[test]
    fn dist_tag_ls_command_is_fully_positional() {
        let ls = npm_dist_tag_ls_command("@scope/app", Path::new("/tmp/.npmrc"), "https://r");
        assert_eq!(
            ls,
            vec![
                "npm",
                "dist-tag",
                "ls",
                "@scope/app",
                "--userconfig",
                "/tmp/.npmrc",
                "--registry",
                "https://r",
            ]
        );
    }

    #[test]
    fn view_optional_deps_command_is_fully_positional() {
        let view = npm_view_optional_deps_command(
            "@scope/app",
            "1.2.3",
            Path::new("/tmp/.npmrc"),
            "https://registry.npmjs.org",
        );
        assert_eq!(
            view,
            vec![
                "npm",
                "view",
                "@scope/app@1.2.3",
                "optionalDependencies",
                "--json",
                "--userconfig",
                "/tmp/.npmrc",
                "--registry",
                "https://registry.npmjs.org",
            ]
        );
    }

    #[test]
    fn parse_dist_tag_version_tolerates_padding_and_missing_lines() {
        // Leading/trailing whitespace around both tag and version is trimmed.
        assert_eq!(
            parse_dist_tag_version("  latest :  9.9.9  \n", "latest"),
            Some("9.9.9".into())
        );
        // The first matching line wins; a blank/garbage line is skipped.
        assert_eq!(
            parse_dist_tag_version("garbage-no-colon\nbeta: 2.0.0\n", "beta"),
            Some("2.0.0".into())
        );
        // No line at all ⇒ None.
        assert_eq!(parse_dist_tag_version("", "latest"), None);
    }

    #[test]
    fn fallback_crate_name_prefers_primary_crate_else_project_name() {
        use anodizer_core::config::CrateConfig;
        use anodizer_core::test_helpers::TestContextBuilder;

        // No crates configured ⇒ falls back to the project name.
        let bare = TestContextBuilder::new().project_name("demo").build();
        assert_eq!(fallback_crate_name(&bare), "demo");

        // A configured crate ⇒ its name is the fallback.
        let with_crate = TestContextBuilder::new()
            .project_name("demo")
            .crates(vec![CrateConfig {
                name: "core".to_string(),
                path: "crates/core".to_string(),
                ..Default::default()
            }])
            .build();
        assert_eq!(fallback_crate_name(&with_crate), "core");
    }

    #[test]
    fn family_label_uses_metapackage_for_optional_deps_and_name_for_postinstall() {
        use anodizer_core::test_helpers::TestContextBuilder;

        let ctx = TestContextBuilder::new().project_name("demo").build();

        // optional-deps: the explicit metapackage names the family.
        let opt = NpmConfig {
            mode: NpmMode::OptionalDeps,
            metapackage: Some("@scope/app".into()),
            ..Default::default()
        };
        assert_eq!(family_label(&ctx, &opt), "@scope/app");

        // postinstall: the `name:` names the family.
        let post = NpmConfig {
            mode: NpmMode::Postinstall,
            name: Some("anodize-demo".into()),
            ..Default::default()
        };
        assert_eq!(family_label(&ctx, &post), "anodize-demo");

        // Neither set ⇒ both modes fall back to the resolved crate/project name.
        let bare = NpmConfig::default();
        assert_eq!(family_label(&ctx, &bare), "demo");
    }

    #[test]
    fn parse_dist_tag_version_picks_named_tag() {
        let out = "latest: 1.2.3\nnext: 1.3.0-rc.1\nbeta: 1.3.0-beta.2\n";
        assert_eq!(
            parse_dist_tag_version(out, "next"),
            Some("1.3.0-rc.1".into())
        );
        assert_eq!(parse_dist_tag_version(out, "latest"), Some("1.2.3".into()));
        assert_eq!(parse_dist_tag_version(out, "missing"), None);
    }

    #[test]
    fn parse_optional_deps_reads_family_and_tolerates_empty() {
        let json = r#"{"@scope/app-linux-x64":"1.2.3","@scope/app-darwin-arm64":"1.2.3"}"#;
        let mut got = parse_optional_deps(json).expect("parse");
        got.sort();
        assert_eq!(
            got,
            vec![
                ("@scope/app-darwin-arm64".to_string(), "1.2.3".to_string()),
                ("@scope/app-linux-x64".to_string(), "1.2.3".to_string()),
            ]
        );
        assert!(parse_optional_deps("").expect("empty").is_empty());
        assert!(parse_optional_deps("undefined").expect("undef").is_empty());
    }

    #[test]
    fn npmrc_cache_distinguishes_tokens_on_same_registry() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::promote::{PromoteRequest, PromoteSelector};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = ctx.logger("npm-promote-test");
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let cfg_dir = TempDir::new().expect("scratch dir");
        let mut retag = ReTagger::new(&req, &log, cfg_dir.path());

        let registry = "https://registry.npmjs.org";
        let a = retag.npmrc(registry, "token-a").expect("npmrc a");
        let b = retag.npmrc(registry, "token-b").expect("npmrc b");
        // Same registry, different token → two distinct `.npmrc` files, so the
        // second target authenticates with its own credential.
        assert_ne!(a, b, "distinct tokens must not share a .npmrc");
        // Re-requesting the first pair returns the cached path (no third file).
        let a_again = retag.npmrc(registry, "token-a").expect("npmrc a cached");
        assert_eq!(a, a_again, "same (registry, token) must reuse its .npmrc");
        assert_eq!(retag.npmrcs.len(), 2, "exactly two cache entries");
    }

    #[test]
    fn recorded_npm_targets_reads_evidence() {
        use anodizer_core::publish_evidence::{NpmExtra, NpmTargetSnapshot};
        use anodizer_core::{
            PublishEvidence, PublishEvidenceExtra, PublisherGroup, PublisherOutcome,
            PublisherResult,
        };

        let mut evidence = PublishEvidence::new("npm");
        evidence.extra = PublishEvidenceExtra::Npm(NpmExtra {
            npm_targets: vec![NpmTargetSnapshot {
                target: "@scope/app".into(),
                package: "@scope/app".into(),
                version: "2.0.0".into(),
                registry: "https://registry.npmjs.org".into(),
                dist_tag: "next".into(),
                token_env_var: "NPM_TOKEN".into(),
            }],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "npm".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });

        assert_eq!(
            recorded_npm_targets(&report),
            vec![(
                "@scope/app".to_string(),
                "2.0.0".to_string(),
                "https://registry.npmjs.org".to_string(),
                "NPM_TOKEN".to_string()
            )]
        );
    }

    #[test]
    fn promoter_name_is_npm() {
        assert_eq!(NpmPromoter::default().name(), "npm");
        assert_eq!(NpmPromoter::new("rc").name(), "npm");
    }

    #[test]
    fn promote_bails_without_any_npms_config() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        // No `npms:` block ⇒ nothing to promote for npm; the verb must bail with
        // an actionable message naming the missing block, before any scratch dir
        // or command.
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: true,
            ctx: &ctx,
        };
        let err = NpmPromoter::default()
            .promote(&req)
            .expect_err("no npms block must bail");
        assert!(
            format!("{err:#}").contains("npms:"),
            "error should name the missing npms block; got {err:#}"
        );
    }

    #[test]
    fn promote_dry_run_names_plan_and_spawns_nothing() {
        use anodizer_core::config::{Config, NpmConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::promote::PromoteStatus;

        let cfg = Config {
            npms: Some(vec![NpmConfig::default()]),
            ..Default::default()
        };
        let ctx = Context::new(cfg, ContextOptions::default());
        // An explicit `--version` selector names the version as the folded
        // `from` label (not the canonical track), and dry-run resolves the plan
        // without spawning `npm`.
        let selector = PromoteSelector::Version("1.4.0".to_string());
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: true,
            ctx: &ctx,
        };
        let out = NpmPromoter::default().promote(&req).expect("dry-run ok");
        assert_eq!(out.status, PromoteStatus::DryRun);
        assert_eq!(out.publisher, "npm");
        assert_eq!(out.from, "1.4.0");
        assert_eq!(out.to, "latest");
        // Dry-run resolves no concrete package set, so `what` is None.
        assert_eq!(out.what, None);
    }

    #[test]
    fn promote_from_run_with_empty_report_is_skipped_nothing_to_promote() {
        use anodizer_core::config::{Config, NpmConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::promote::{PromoteSkipReason, PromoteStatus};

        // FromRun with a report holding no npm targets ⇒ the recorded family is
        // empty ⇒ nothing is re-tagged and no `npm` command spawns; the outcome
        // is Skipped(NothingToPromote), and the `from` label is the run id.
        let cfg = Config {
            npms: Some(vec![NpmConfig::default()]),
            ..Default::default()
        };
        let ctx = Context::new(cfg, ContextOptions::default());
        let selector = PromoteSelector::FromRun {
            run_id: "abc123".to_string(),
            report: PublishReport::default(),
        };
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let out = NpmPromoter::default()
            .promote(&req)
            .expect("empty recorded family ok");
        assert_eq!(
            out.status,
            PromoteStatus::Skipped(PromoteSkipReason::NothingToPromote)
        );
        assert_eq!(out.from, "run abc123");
    }

    #[test]
    fn env_token_reads_named_var_and_rejects_unset_or_empty() {
        use anodizer_core::test_helpers::TestContextBuilder;

        // Sealed env so the assertion reflects only the injected fixture, never
        // the host's ambient NPM_TOKEN.
        let ctx = TestContextBuilder::new()
            .env("NPM_TOKEN", "s3cr3t")
            .env("EMPTY_TOKEN", "")
            .build();
        let log = ctx.logger("npm-promote-test");
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let cfg_dir = TempDir::new().expect("scratch dir");
        let retag = ReTagger::new(&req, &log, cfg_dir.path());

        assert_eq!(retag.env_token("NPM_TOKEN").expect("set var"), "s3cr3t");
        // An empty value counts as unset — the error names the offending var.
        let empty_err = retag
            .env_token("EMPTY_TOKEN")
            .expect_err("empty value rejected");
        assert!(
            format!("{empty_err:#}").contains("EMPTY_TOKEN"),
            "error should name the empty var; got {empty_err:#}"
        );
        let missing_err = retag
            .env_token("UNSET_VAR")
            .expect_err("unset var rejected");
        assert!(
            format!("{missing_err:#}").contains("UNSET_VAR"),
            "error should name the unset var; got {missing_err:#}"
        );
    }

    #[test]
    fn resolve_version_returns_explicit_version_selector_verbatim() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        // The `Version` selector short-circuits — the version is returned as-is
        // with no registry round-trip (no `npm dist-tag ls` spawn).
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = ctx.logger("npm-promote-test");
        let selector = PromoteSelector::Version("9.9.9".to_string());
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let cfg_dir = TempDir::new().expect("scratch dir");
        let retag = ReTagger::new(&req, &log, cfg_dir.path());
        let v = retag
            .resolve_version("pkg", "https://registry.npmjs.org", Path::new("/x/.npmrc"))
            .expect("resolve version");
        assert_eq!(v, Some("9.9.9".to_string()));
    }

    #[test]
    fn retag_recorded_routes_every_target_to_failed_when_token_env_unset() {
        use anodizer_core::publish_evidence::{NpmExtra, NpmTargetSnapshot};
        use anodizer_core::test_helpers::TestContextBuilder;
        use anodizer_core::{PublishEvidence, PublisherGroup, PublisherOutcome, PublisherResult};

        // Sealed empty env ⇒ the recorded target's token var resolves to nothing,
        // so retag records a per-target failure WITHOUT spawning `npm dist-tag
        // add` (env-token resolution fails before the subprocess).
        let ctx = TestContextBuilder::new().sealed_env().build();
        let log = ctx.logger("npm-promote-test");
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "next".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let cfg_dir = TempDir::new().expect("scratch dir");
        let mut retag = ReTagger::new(&req, &log, cfg_dir.path());

        let mut evidence = PublishEvidence::new("npm");
        evidence.extra = PublishEvidenceExtra::Npm(NpmExtra {
            npm_targets: vec![NpmTargetSnapshot {
                target: "pkg".into(),
                package: "pkg".into(),
                version: "1.0.0".into(),
                registry: "https://registry.npmjs.org".into(),
                dist_tag: "next".into(),
                token_env_var: "DEFINITELY_UNSET_NPM_TOKEN".into(),
            }],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "npm".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });

        retag.retag_recorded(&report);
        assert!(
            retag.applied.is_empty(),
            "no package should re-tag with an unresolvable token"
        );
        assert_eq!(retag.failed.len(), 1, "the single target must be a failure");
        assert!(
            retag.failed[0].0.contains("pkg@1.0.0"),
            "failure label should be the package@version; got {}",
            retag.failed[0].0
        );
        assert!(
            retag.failed[0].1.contains("DEFINITELY_UNSET_NPM_TOKEN"),
            "failure cause should name the unresolvable token var; got {}",
            retag.failed[0].1
        );
    }
}
