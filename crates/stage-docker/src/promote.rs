//! OCI floating-tag promotion — the [`Promotable`] implementation for docker.
//!
//! Re-points a floating tag at an already-pushed image with
//! `docker buildx imagetools create --tag <repo>:<to> <repo>:<from>` — a
//! registry-side manifest copy, no rebuild and no re-push. Every configured
//! image repo is re-pointed so a multi-image project promotes as one unit.
//!
//! The source reference is resolved from the [`PromoteSelector`]:
//! * [`PromoteSelector::Version`] → the immutable version-bearing tag the build
//!   stage pushed for that version, reconstructed by rendering the config's
//!   `tags[]` templates (a project whose `tags:` is `{{ .Tag }}` or
//!   `v{{ .Version }}` never pushed the bare `<repo>:<version>`, so promoting
//!   that literal would 404). Errors when the config carries only floating tags
//!   (`latest`/`edge`): there is no immutable per-version coordinate to promote
//!   from, so a version-pinned promotion cannot be sourced.
//! * [`PromoteSelector::Newest`] → `<repo>:<from>` (the floating from-track tag
//!   as-is).
//! * [`PromoteSelector::FromRun`] → `<repo>:<from>`. Docker image pushes are a
//!   build-stage artifact, not a publisher, so they are not recorded in the
//!   run's publish report; the from-track floating tag the prior run left is
//!   the promotable coordinate.
//!
//! Every configured image repo is re-pointed best-effort: a failure on one repo
//! is collected and the remaining repos are still attempted, then the run fails
//! naming both what was already re-pointed and what failed.

use std::process::Command;

use anodizer_core::config::DockerV2Config;
use anodizer_core::context::Context;
use anodizer_core::git::per_crate_tag_prefix;
use anodizer_core::log::StageLogger;
use anodizer_core::promote::{
    Promotable, PromoteOutcome, PromoteRequest, PromoteSelector, is_canonical_pretrack,
    partial_promotion_error,
};
use anodizer_core::run::run_checked;
use anyhow::{Context as _, Result, bail};

use crate::command::is_docker_v2_skipped;

/// The tag npm's canonical `stable` track maps to for docker.
const STABLE_TAG: &str = "latest";
/// The tag the pre-stable aliases map to.
const EDGE_TAG: &str = "edge";

/// The docker/OCI promotion capability. Zero-sized; all state comes from the
/// [`PromoteRequest`]'s [`Context`].
pub struct DockerPromoter;

impl Promotable for DockerPromoter {
    fn name(&self) -> &str {
        "docker"
    }

    /// A floating tag has no fixed vocabulary — the canonical track name *is* a
    /// tag. `stable` maps to the conventional `latest`, and every canonical
    /// pre-stable alias (`prerelease`/`candidate`/`beta`/`edge`) maps to the
    /// single native pre-track `edge`. Anything else — including a raw tag the
    /// operator typed — passes through verbatim.
    fn resolve_track(&self, canonical: &str) -> String {
        if canonical == "stable" {
            STABLE_TAG.to_string()
        } else if is_canonical_pretrack(canonical) {
            EDGE_TAG.to_string()
        } else {
            canonical.to_string()
        }
    }

    fn promote(&self, req: &PromoteRequest) -> Result<PromoteOutcome> {
        let log = req.ctx.logger("docker-promote");

        let repos = resolve_image_repos(req.ctx)?;
        if repos.is_empty() {
            bail!(
                "no docker image repo resolved; \
                 `anodizer promote --publishers docker` needs a `dockers_v2:` block with `images:`"
            );
        }

        // The `from` shown in the folded outcome names the source the selector
        // actually targets (`--version`/`--from-run`), not the canonical track.
        let from_label = req.selector.source_label(&req.from);

        if req.dry_run {
            for repo in &repos {
                let source_tag = resolve_source_tag(repo, req)?;
                log.status(&format!(
                    "(dry-run) would promote docker {}:{source_tag} → {}:{}",
                    repo.repo, repo.repo, req.to
                ));
            }
            return Ok(PromoteOutcome::dry_run(
                self.name(),
                from_label,
                &req.to,
                Some(format!("{} image(s)", repos.len())),
            ));
        }

        let mut applied: Vec<String> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        for repo in &repos {
            match promote_one(repo, req, &log) {
                Ok(()) => applied.push(repo.repo.clone()),
                Err(err) => failed.push((repo.repo.clone(), format!("{err:#}"))),
            }
        }

        if !failed.is_empty() {
            bail!("{}", partial_promotion_error(&applied, &failed));
        }

        Ok(PromoteOutcome::promoted(
            self.name(),
            from_label,
            &req.to,
            format!("{} image(s)", applied.len()),
        ))
    }
}

/// Re-point one image repo's destination tag at its selector-resolved source.
fn promote_one(repo: &DockerRepo, req: &PromoteRequest, log: &StageLogger) -> Result<()> {
    let source_tag = resolve_source_tag(repo, req)?;
    let source = format!("{}:{source_tag}", repo.repo);
    let dest = format!("{}:{}", repo.repo, req.to);
    let args = imagetools_create_command(&dest, &source);
    log.verbose(&format!("running {}", args.join(" ")));
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    run_checked(&mut cmd, log, "docker buildx imagetools create")
        .with_context(|| format!("failed to re-point {dest} at {source}"))?;
    log.status(&format!("promoted docker {source} → {dest}"));
    Ok(())
}

/// Resolve the source tag one repo is promoted FROM. `Version` reconstructs the
/// immutable pushed tag by rendering the config's `tags[]`; `Newest`/`FromRun`
/// promote the floating from-track tag as-is (docker pushes carry no per-run
/// recorded coordinate).
fn resolve_source_tag(repo: &DockerRepo, req: &PromoteRequest) -> Result<String> {
    match req.selector {
        PromoteSelector::Version(v) => {
            let tag = format!("{}{v}", repo.tag_prefix);
            let mut rendered_tags: Vec<String> = Vec::new();
            for tmpl in &repo.tags {
                // Render with the FULL target-version var set so a tag template
                // using `{{ .Major }}`/`{{ .Minor }}`/`{{ .Patch }}`/`{{ .RawVersion }}`
                // reconstructs the tag for `v`, not the context version.
                let rendered = req
                    .ctx
                    .render_template_for_version(tmpl, v, &tag)
                    .with_context(|| {
                        format!("dockers_v2: render tag template '{tmpl}' for promotion")
                    })?;
                rendered_tags.push(rendered);
            }
            match select_version_source_tag(&rendered_tags, v) {
                Some(tag) => Ok(tag),
                None => bail!(
                    "dockers_v2: cannot promote --version {v} for '{repo}': none of the \
                     configured tag templates {tags:?} produce a version-bearing immutable \
                     tag (floating-only tags like `latest`/`edge` carry no per-version \
                     coordinate to promote from)",
                    repo = repo.repo,
                    tags = repo.tags,
                ),
            }
        }
        _ => Ok(req.from.clone()),
    }
}

/// From a config's `tags[]` rendered for `version`, pick the immutable source
/// tag: the first rendered tag that embeds the version (the version-bearing
/// immutable tag the release pushed). Returns `None` when no rendered tag embeds
/// the version (a floating-only config, e.g. `tags: [latest, edge]`) — there is
/// no immutable per-version coordinate to promote from, so the caller must error
/// rather than fabricate a never-pushed `<repo>:<version>`.
fn select_version_source_tag(rendered_tags: &[String], version: &str) -> Option<String> {
    rendered_tags
        .iter()
        .map(|t| t.trim())
        // Version-bearing = the rendered tag embeds (or exactly equals) the
        // version, so it is the immutable tag the release pushed.
        .find(|t| t.contains(version) || *t == version)
        .map(str::to_string)
}

/// One image repo to promote, paired with the metadata needed to reconstruct
/// the immutable version tag a prior release pushed for an explicit `--version`.
struct DockerRepo {
    /// Fully-rendered repo (`ghcr.io/owner/app`).
    repo: String,
    /// The owning config's `tags[]` templates — rendered with a `--version`
    /// override to find the version-bearing immutable tag.
    tags: Vec<String>,
    /// Tag-family prefix from the owning crate's `tag_template` (e.g. `v`), used
    /// to reconstruct the `{{ .Tag }}` a release stamped for a version.
    tag_prefix: String,
}

/// Resolve the fully-rendered image repos across every non-skipped
/// `dockers_v2` config in the crate universe (deduplicated by repo,
/// order-stable). A repo is the `images[]` entry with its registry/owner
/// embedded (`ghcr.io/owner/app`); each is paired with its owning config's
/// `tags[]` and the crate's tag prefix so a `--version` promotion can
/// reconstruct the immutable pushed tag.
fn resolve_image_repos(ctx: &Context) -> Result<Vec<DockerRepo>> {
    let mut repos: Vec<DockerRepo> = Vec::new();
    for krate in ctx.config.crate_universe() {
        let Some(dockers) = krate.dockers_v2.as_ref() else {
            continue;
        };
        let tag_prefix = per_crate_tag_prefix(&krate.name, &krate.tag_template);
        for cfg in dockers {
            if is_docker_v2_skipped(&cfg.skip, ctx)? {
                continue;
            }
            for repo in render_repos(ctx, cfg, &krate.name)? {
                if !repos.iter().any(|r| r.repo == repo) {
                    repos.push(DockerRepo {
                        repo,
                        tags: cfg.tags.clone(),
                        tag_prefix: tag_prefix.clone(),
                    });
                }
            }
        }
    }
    Ok(repos)
}

/// Render one config's `images[]` templates to concrete repos, dropping empties.
fn render_repos(ctx: &Context, cfg: &DockerV2Config, crate_name: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for img_tmpl in &cfg.images {
        let rendered = ctx.render_template(img_tmpl).with_context(|| {
            format!("dockers_v2: render image template '{img_tmpl}' for crate {crate_name}")
        })?;
        let rendered = rendered.trim();
        if !rendered.is_empty() {
            out.push(rendered.to_string());
        }
    }
    Ok(out)
}

/// `docker buildx imagetools create --tag <dest> <source>` — copy the `source`
/// manifest to the `dest` tag registry-side, no rebuild.
fn imagetools_create_command(dest: &str, source: &str) -> Vec<String> {
    vec![
        "docker".to_string(),
        "buildx".to_string(),
        "imagetools".to_string(),
        "create".to_string(),
        "--tag".to_string(),
        dest.to_string(),
        source.to_string(),
    ]
}

/// Preflight for docker promotion: `docker buildx` must be available —
/// `imagetools create` is a buildx subcommand. Called by the verb only when
/// docker is among the selected publishers.
pub fn preflight() -> Result<()> {
    if !anodizer_core::docker_detect::buildx_available().unwrap_or(false) {
        bail!(
            "`docker buildx` not available — OCI tag promotion runs \
             `docker buildx imagetools create`; install docker with the buildx \
             plugin or deselect it with --publishers"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_track_maps_canonical_else_identity() {
        let p = DockerPromoter;
        assert_eq!(p.resolve_track("stable"), "latest");
        // Every canonical pre-stable alias maps to the single native pre-track.
        assert_eq!(p.resolve_track("prerelease"), "edge");
        assert_eq!(p.resolve_track("candidate"), "edge");
        assert_eq!(p.resolve_track("beta"), "edge");
        assert_eq!(p.resolve_track("edge"), "edge");
        // A raw tag passes through verbatim.
        assert_eq!(p.resolve_track("v1.2.3"), "v1.2.3");
        assert_eq!(p.resolve_track("nightly"), "nightly");
    }

    #[test]
    fn select_version_source_tag_prefers_version_bearing_tag() {
        // `{{ .Tag }}` with prefix `v` renders `v1.4.0` — the immutable tag.
        assert_eq!(
            select_version_source_tag(&["v1.4.0".to_string()], "1.4.0"),
            Some("v1.4.0".to_string())
        );
        // `{{ .Version }}` + a floating tag: the version-bearing one wins.
        assert_eq!(
            select_version_source_tag(&["1.4.0".to_string(), "latest".to_string()], "1.4.0"),
            Some("1.4.0".to_string())
        );
        // Only floating tags — no immutable per-version coordinate exists, so
        // there is nothing to source (caller turns this into a hard error).
        assert_eq!(
            select_version_source_tag(&["latest".to_string(), "edge".to_string()], "1.4.0"),
            None
        );
    }

    #[test]
    fn resolve_source_tag_errors_on_floating_only_version_promotion() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::promote::{PromoteRequest, PromoteSelector};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        let repo = DockerRepo {
            repo: "ghcr.io/o/app".to_string(),
            // Floating-only config — no template embeds the version.
            tags: vec!["latest".to_string()],
            tag_prefix: "v".to_string(),
        };
        let selector = PromoteSelector::Version("1.4.0".to_string());
        let req = PromoteRequest {
            from: "edge".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };

        let err = resolve_source_tag(&repo, &req).expect_err("floating-only must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("ghcr.io/o/app"), "error names the repo: {msg}");
        assert!(msg.contains("floating"), "error mentions floating: {msg}");
    }

    #[test]
    fn resolve_source_tag_uses_target_version_for_semver_parts() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::promote::{PromoteRequest, PromoteSelector};

        // Context version is 2.0.0 — the WRONG source for the target's tag.
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let vars = ctx.template_vars_mut();
        vars.set("Version", "2.0.0");
        vars.set("Major", "2");
        vars.set("Minor", "0");
        vars.set("Patch", "0");

        let repo = DockerRepo {
            repo: "ghcr.io/o/app".to_string(),
            tags: vec!["v{{ .Major }}.{{ .Minor }}.{{ .Patch }}".to_string()],
            tag_prefix: "v".to_string(),
        };
        let selector = PromoteSelector::Version("1.4.0".to_string());
        let req = PromoteRequest {
            from: "edge".to_string(),
            to: "latest".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };

        // The target version (1.4.0) — not the context version (2.0.0) — must
        // drive Major/Minor/Patch, so the immutable source tag is v1.4.0.
        assert_eq!(resolve_source_tag(&repo, &req).expect("resolve"), "v1.4.0");
    }

    #[test]
    fn imagetools_create_command_shape() {
        let args = imagetools_create_command("ghcr.io/o/a:latest", "ghcr.io/o/a:edge");
        assert_eq!(
            args,
            vec![
                "docker",
                "buildx",
                "imagetools",
                "create",
                "--tag",
                "ghcr.io/o/a:latest",
                "ghcr.io/o/a:edge",
            ]
        );
    }

    #[test]
    fn resolve_image_repos_dedups_and_skips() {
        use anodizer_core::config::{Config, CrateConfig, StringOrBool, WorkspaceConfig};
        use anodizer_core::context::ContextOptions;

        fn docker_crate(name: &str, images: Vec<&str>, skip: bool) -> CrateConfig {
            CrateConfig {
                name: name.to_string(),
                path: ".".to_string(),
                dockers_v2: Some(vec![DockerV2Config {
                    dockerfile: "Dockerfile".to_string(),
                    images: images.into_iter().map(String::from).collect(),
                    tags: vec!["latest".to_string()],
                    skip: skip.then_some(StringOrBool::Bool(true)),
                    ..Default::default()
                }]),
                ..Default::default()
            }
        }

        // Workspace mode with two members sharing one repo plus a skipped one.
        let config = Config {
            project_name: "ws".to_string(),
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![
                    docker_crate("a", vec!["ghcr.io/o/app"], false),
                    docker_crate("b", vec!["ghcr.io/o/app", "ghcr.io/o/extra"], false),
                    docker_crate("c", vec!["ghcr.io/o/skipped"], true),
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = Context::new(config, ContextOptions::default());

        let repos = resolve_image_repos(&ctx).expect("resolve");
        let names: Vec<&str> = repos.iter().map(|r| r.repo.as_str()).collect();
        assert_eq!(names, vec!["ghcr.io/o/app", "ghcr.io/o/extra"]);
    }
}
