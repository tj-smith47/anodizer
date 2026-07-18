//! GitHub release promotion — the [`Promotable`] implementation for the
//! `github` publisher.
//!
//! Flips an already-published release from prerelease to stable/latest through
//! the GitHub REST API — `PATCH /repos/{owner}/{repo}/releases/{id}` with
//! `{"prerelease": false, "make_latest": "true"}`. Unlike the snapcraft / npm /
//! docker promoters this runs **no subprocess**: a "track" for GitHub is the
//! release's `prerelease` flag, not a channel string, so promotion is a single
//! API mutation. The reverse direction (promoting *to* a pre-track) sets
//! `prerelease: true` and leaves `make_latest` untouched.
//!
//! The release is located from the [`PromoteSelector`]:
//! * [`PromoteSelector::Version`] → the release tagged with that version.
//! * [`PromoteSelector::FromRun`] → the tag the prior run recorded in its
//!   github-release [`PublishEvidence`].
//! * [`PromoteSelector::Newest`] → the newest release still flagged
//!   `prerelease` (and not a draft).
//!
//! [`PublishEvidence`]: anodizer_core::PublishEvidence

use std::sync::Arc;

use anodizer_core::config::ReleaseConfig;
use anodizer_core::context::Context;
use anodizer_core::promote::{
    Promotable, PromoteOutcome, PromoteRequest, PromoteSelector, PromoteSkipReason,
    is_canonical_pretrack, partial_promotion_error,
};
use anodizer_core::scm::ScmTokenType;
use anodizer_core::{PublishEvidenceExtra, PublishReport};
use anyhow::{Context as _, Result, anyhow, bail};
use octocrab::models::repos::Release;

use crate::github::{build_octocrab_client, is_octocrab_404, retry_octocrab_call};

/// Canonical track that promotes a release to non-prerelease + latest.
const STABLE_TRACK: &str = "stable";

/// The GitHub release promotion capability. Zero-sized; all state comes from
/// the [`PromoteRequest`]'s [`Context`].
pub struct GithubReleasePromoter;

impl Promotable for GithubReleasePromoter {
    fn name(&self) -> &str {
        "github"
    }

    /// GitHub has no channel vocabulary — a release is either `prerelease` or
    /// not. `stable` normalizes to `stable` (interpreted as "clear the
    /// prerelease flag + make latest"); every pre-stable alias normalizes to
    /// `prerelease`. A raw value passes through verbatim; anything other than
    /// `stable` is treated as a pre-track by [`promote`](Self::promote).
    fn resolve_track(&self, canonical: &str) -> String {
        if canonical == "stable" {
            STABLE_TRACK.to_string()
        } else if is_canonical_pretrack(canonical) {
            "prerelease".to_string()
        } else {
            canonical.to_string()
        }
    }

    fn promote(&self, req: &PromoteRequest) -> Result<PromoteOutcome> {
        let log = req.ctx.logger("github-promote");
        let to_stable = req.to.eq_ignore_ascii_case(STABLE_TRACK);

        let repos = resolve_github_repos(req.ctx)?;
        if repos.is_empty() {
            bail!(
                "no github release repo resolved from any `release.github` block; \
                 `anodizer promote --publishers github` needs a `release.github` block"
            );
        }

        // The `from` shown in the folded outcome names the source the selector
        // actually targets (`--version`/`--from-run`), not the canonical track.
        let from_label = req.selector.source_label(&req.from);

        if req.dry_run {
            for repo in &repos {
                log.status(&format!(
                    "(dry-run) would flip github release {} on {}/{} ({}→{})",
                    req.selector.describe(),
                    repo.owner,
                    repo.name,
                    req.from,
                    req.to
                ));
            }
            return Ok(PromoteOutcome::dry_run(
                self.name(),
                from_label,
                &req.to,
                Some(format!("{} release(s)", repos.len())),
            ));
        }

        let rt = tokio::runtime::Runtime::new().context("github-promote: create tokio runtime")?;
        let policy = req.ctx.retry_policy();
        let deadline = req.ctx.retry_deadline();

        // Best-effort across repos: a per-repo failure is collected and the
        // remaining repos are still attempted, then the run fails naming both
        // what was already flipped and what failed. A repo whose selector
        // matched no release is "nothing to promote" for THAT repo (not a
        // failure); if every repo matched nothing, the outcome is skipped.
        let mut promoted: Vec<String> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        for repo in &repos {
            let label = format!("{}/{}", repo.owner, repo.name);
            match flip_one(repo, req, &rt, &policy, deadline, to_stable, &log) {
                Ok(Some(tag)) => promoted.push(format!("{label}@{tag}")),
                Ok(None) => {}
                Err(err) => failed.push((label, format!("{err:#}"))),
            }
        }

        if !failed.is_empty() {
            bail!("{}", partial_promotion_error(&promoted, &failed));
        }

        if promoted.is_empty() {
            log.status(&format!(
                "no github release matched {} — nothing to promote",
                req.selector.describe()
            ));
            return Ok(PromoteOutcome::skipped(
                self.name(),
                from_label,
                &req.to,
                PromoteSkipReason::NothingToPromote,
            ));
        }

        Ok(PromoteOutcome::promoted(
            self.name(),
            from_label,
            &req.to,
            format!("{} release(s)", promoted.len()),
        ))
    }
}

/// Flip one repo's selector-matched release. `Ok(Some(tag))` = flipped;
/// `Ok(None)` = no release matched (nothing to promote for this repo);
/// `Err` = the API mutation (or credential resolution) failed for this repo.
#[allow(clippy::too_many_arguments)]
fn flip_one(
    repo: &GithubTarget,
    req: &PromoteRequest,
    rt: &tokio::runtime::Runtime,
    policy: &anodizer_core::retry::RetryPolicy,
    deadline: Option<std::time::Instant>,
    to_stable: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<Option<String>> {
    // The plan is token-independent (so dry-run renders without credentials);
    // a live PATCH needs the token. The verb preflight fails fast when any
    // target is untokened, so this is defensive.
    let Some(token) = repo.token.as_deref() else {
        bail!(
            "github-promote: no GitHub token resolved for {}/{} — set \
             ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN / GH_TOKEN (or pass --token)",
            repo.owner,
            repo.name
        );
    };
    let (octo_raw, retry_after) = build_octocrab_client(token, &req.ctx.config.github_urls)?;
    let octo = Arc::new(octo_raw);

    let flipped = rt.block_on(async {
        let Some(release) =
            locate_release(&octo, repo, req, policy, to_stable, Some(&retry_after)).await?
        else {
            return Ok::<Option<String>, anyhow::Error>(None);
        };
        let id = release.id.into_inner();
        let tag = release.tag_name.clone();
        let body = patch_body(to_stable);
        let route = format!("/repos/{}/{}/releases/{}", repo.owner, repo.name, id);
        retry_octocrab_call(
            policy,
            deadline,
            "promote release PATCH",
            Some(&retry_after),
            || {
                let octo = octo.clone();
                let route = route.clone();
                let body = body.clone();
                async move { octo.patch::<Release, _, _>(route, Some(&body)).await }
            },
        )
        .await
        .with_context(|| {
            format!(
                "github-promote: PATCH release '{tag}' on {}/{}",
                repo.owner, repo.name
            )
        })?;
        Ok(Some(tag))
    })?;

    if let Some(tag) = &flipped {
        log.status(&format!(
            "promoted github release {tag} on {}/{} {}→{}",
            repo.owner, repo.name, req.from, req.to
        ));
    }
    Ok(flipped)
}

/// A GitHub release repo to promote in: owner, name, and the resolved token.
///
/// `token` is `None` when no credential could be resolved from config/env. The
/// *plan* (owner/name) is resolved independently of the token so a dry-run can
/// render it without credentials; a live promotion requires the token, enforced
/// by [`preflight`] (fail-fast) and re-checked in [`GithubReleasePromoter::promote`].
struct GithubTarget {
    owner: String,
    name: String,
    token: Option<String>,
}

/// Resolve every distinct GitHub release repo across the crate universe (all
/// three config modes flow through `crate_universe`), pairing each with its
/// resolved token *when available*. A crate whose release provider is not
/// GitHub is skipped; a repo with no resolvable token is kept with
/// `token: None` (the plan is still valid for a dry-run). Deduplicated by
/// `owner/name` so a lockstep workspace sharing one repo/tag flips it once.
fn resolve_github_repos(ctx: &Context) -> Result<Vec<GithubTarget>> {
    let mut out: Vec<GithubTarget> = Vec::new();
    for krate in ctx.config.crate_universe() {
        let Some(release_cfg) = krate.release.as_ref() else {
            continue;
        };
        let Some(repo) = anodizer_core::download_url::resolve_release_repo(
            release_cfg,
            ScmTokenType::GitHub,
            ctx,
        )?
        else {
            continue;
        };
        let token = resolve_token(ctx, release_cfg, &repo);
        if let Some(existing) = out
            .iter_mut()
            .find(|t| t.owner == repo.owner && t.name == repo.name)
        {
            // Prefer a tokened entry: if an earlier crate contributed this repo
            // untokened and a later crate carries a token for it, adopt the
            // token so preflight (which requires every target be tokened) passes.
            if existing.token.is_none() && token.is_some() {
                existing.token = token;
            }
            continue;
        }
        out.push(GithubTarget {
            owner: repo.owner,
            name: repo.name,
            token,
        });
    }
    Ok(out)
}

/// Resolve the GitHub token: the per-repo `release.github.token`, else the
/// release-stage token ladder, else the pipeline-resolved `--token`.
fn resolve_token(
    ctx: &Context,
    release_cfg: &ReleaseConfig,
    repo: &anodizer_core::config::ScmRepoConfig,
) -> Option<String> {
    repo.token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| crate::resolve_release_token(ctx, release_cfg))
        .or_else(|| ctx.options.token.clone())
        .filter(|t| !t.is_empty())
}

/// Locate the release to promote for one repo from the selector.
/// `Ok(None)` = no matching release (nothing to promote for this repo).
///
/// `to_stable` makes the `Newest` selector direction-aware: promoting TO stable
/// picks the newest CURRENT prerelease; promoting to a pre-track (a reverse
/// promotion, e.g. demoting a mistakenly-stabilized release) picks the newest
/// CURRENT non-prerelease. Without this a reverse promotion could never find a
/// stable release to flip and silently reported "nothing to promote".
async fn locate_release(
    octo: &Arc<octocrab::Octocrab>,
    repo: &GithubTarget,
    req: &PromoteRequest<'_>,
    policy: &anodizer_core::retry::RetryPolicy,
    to_stable: bool,
    retry_after: Option<&crate::github::RetryAfterCapture>,
) -> Result<Option<Release>> {
    match req.selector {
        PromoteSelector::Version(v) => get_release_by_tag(octo, repo, v, policy, retry_after).await,
        PromoteSelector::FromRun { report, .. } => {
            match recorded_tag(report, &repo.owner, &repo.name) {
                Some(tag) => get_release_by_tag(octo, repo, &tag, policy, retry_after).await,
                None => Ok(None),
            }
        }
        PromoteSelector::Newest => newest_release(octo, repo, policy, to_stable, retry_after).await,
    }
}

/// `GET /repos/{owner}/{repo}/releases/tags/{tag}`, mapping a 404 to `None`.
async fn get_release_by_tag(
    octo: &Arc<octocrab::Octocrab>,
    repo: &GithubTarget,
    tag: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&crate::github::RetryAfterCapture>,
) -> Result<Option<Release>> {
    let owner = repo.owner.clone();
    let name = repo.name.clone();
    let tag = tag.to_string();
    let result = retry_octocrab_call(policy, None, "promote get release", retry_after, || {
        let octo = octo.clone();
        let owner = owner.clone();
        let name = name.clone();
        let tag = tag.clone();
        async move { octo.repos(&owner, &name).releases().get_by_tag(&tag).await }
    })
    .await;
    match result {
        Ok(release) => Ok(Some(release)),
        Err(err) if is_octocrab_404(&err) => Ok(None),
        Err(err) => Err(anyhow!(err)),
    }
}

/// The newest release whose CURRENT state is the opposite of the promotion
/// target (never a draft). GitHub returns the first page newest-first, so the
/// first match is the newest. Promoting TO stable finds the newest current
/// prerelease (the release to stabilize); promoting to a pre-track finds the
/// newest current non-prerelease (a reverse promotion / demotion), which the
/// old prerelease-only filter could never locate.
async fn newest_release(
    octo: &Arc<octocrab::Octocrab>,
    repo: &GithubTarget,
    policy: &anodizer_core::retry::RetryPolicy,
    to_stable: bool,
    retry_after: Option<&crate::github::RetryAfterCapture>,
) -> Result<Option<Release>> {
    let route = format!(
        "/repos/{}/{}/releases?per_page=100&page=1",
        repo.owner, repo.name
    );
    let releases: Vec<Release> =
        retry_octocrab_call(policy, None, "promote list releases", retry_after, || {
            let octo = octo.clone();
            let route = route.clone();
            async move { octo.get(route, None::<&()>).await }
        })
        .await
        .with_context(|| {
            format!(
                "github-promote: list releases on {}/{}",
                repo.owner, repo.name
            )
        })?;
    Ok(releases
        .into_iter()
        .find(|r| newest_direction_matches(r.prerelease, r.draft, to_stable)))
}

/// Whether a release is a candidate for the direction-aware `Newest` selector:
/// not a draft, and its current `prerelease` flag is the OPPOSITE of the target
/// (promote to stable → wants a current prerelease; promote to a pre-track →
/// wants a current non-prerelease). Pure over the two release booleans so the
/// direction rule is unit-testable without a live GitHub API.
fn newest_direction_matches(prerelease: bool, draft: bool, to_stable: bool) -> bool {
    !draft && prerelease == to_stable
}

/// Build the PATCH body. Stable target clears the prerelease flag and requests
/// latest; a pre-track target sets the prerelease flag (and leaves make_latest
/// unset, since a prerelease can never be latest).
fn patch_body(to_stable: bool) -> serde_json::Value {
    if to_stable {
        serde_json::json!({ "prerelease": false, "make_latest": "true" })
    } else {
        serde_json::json!({ "prerelease": true })
    }
}

/// Pull the recorded release tag for `owner/repo` out of a prior run's
/// github-release [`PublishEvidence`].
fn recorded_tag(report: &PublishReport, owner: &str, repo: &str) -> Option<String> {
    report
        .results
        .iter()
        .filter(|r| r.name == "github-release")
        .filter_map(|r| r.evidence.as_ref())
        .filter_map(|e| match &e.extra {
            PublishEvidenceExtra::GithubRelease(g) => Some(&g.github_release_targets),
            _ => None,
        })
        .flatten()
        .find(|t| t.owner == owner && t.repo == repo)
        .map(|t| t.tag.clone())
}

/// Preflight for GitHub release promotion: a `release.github` block must
/// resolve and every target repo must have a resolvable token. API-only — no
/// external tool to probe. Called by the verb only for a live run (skipped in
/// dry-run) and only when github is among the selected publishers.
pub fn preflight(ctx: &Context) -> Result<()> {
    let repos = resolve_github_repos(ctx)?;
    if repos.is_empty() {
        bail!(
            "no `release.github` block resolved for release promotion — \
             configure a github release repo, or deselect github with --publishers"
        );
    }
    let untokened: Vec<String> = repos
        .iter()
        .filter(|t| t.token.is_none())
        .map(|t| format!("{}/{}", t.owner, t.name))
        .collect();
    if !untokened.is_empty() {
        bail!(
            "no GitHub token resolved for {} — set \
             ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN / GH_TOKEN (or pass --token), \
             or deselect github with --publishers",
            untokened.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_track_normalizes_to_prerelease_or_stable() {
        let p = GithubReleasePromoter;
        assert_eq!(p.resolve_track("stable"), "stable");
        assert_eq!(p.resolve_track("prerelease"), "prerelease");
        assert_eq!(p.resolve_track("beta"), "prerelease");
        assert_eq!(p.resolve_track("candidate"), "prerelease");
        assert_eq!(p.resolve_track("edge"), "prerelease");
        // Unknown raw value passes through (treated as a pre-track by promote).
        assert_eq!(p.resolve_track("rc"), "rc");
    }

    #[test]
    fn newest_direction_is_direction_aware() {
        // Promote TO stable: pick the newest CURRENT prerelease; a stable
        // release is not a candidate.
        assert!(newest_direction_matches(true, false, true));
        assert!(!newest_direction_matches(false, false, true));
        // Reverse promotion (target a pre-track): pick the newest CURRENT
        // non-prerelease; a prerelease is not a candidate.
        assert!(newest_direction_matches(false, false, false));
        assert!(!newest_direction_matches(true, false, false));
        // Drafts are never candidates in either direction.
        assert!(!newest_direction_matches(true, true, true));
        assert!(!newest_direction_matches(false, true, false));
    }

    #[test]
    fn patch_body_stable_clears_prerelease_and_makes_latest() {
        let b = patch_body(true);
        assert_eq!(b["prerelease"], serde_json::json!(false));
        assert_eq!(b["make_latest"], serde_json::json!("true"));
    }

    #[test]
    fn patch_body_pretrack_sets_prerelease_no_make_latest() {
        let b = patch_body(false);
        assert_eq!(b["prerelease"], serde_json::json!(true));
        assert!(b.get("make_latest").is_none());
    }

    #[test]
    fn resolve_github_repos_resolves_plan_without_a_token() {
        use anodizer_core::config::ReleaseConfig;
        use anodizer_core::config::{Config, CrateConfig, ScmRepoConfig, WorkspaceConfig};
        use anodizer_core::context::{Context, ContextOptions};

        fn github_crate(name: &str, owner: &str, repo: &str, token: Option<&str>) -> CrateConfig {
            CrateConfig {
                name: name.to_string(),
                path: ".".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: owner.to_string(),
                        name: repo.to_string(),
                        token: token.map(String::from),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        let config = Config {
            project_name: "ws".to_string(),
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![
                    // No token in config/CLI — the plan must still resolve (the fix:
                    // repo resolution is decoupled from token resolution so a
                    // dry-run renders without credentials).
                    github_crate("a", "acme", "app", None),
                    // Explicit per-repo token — attached to the target.
                    github_crate("b", "acme", "other", Some("ghp_explicit")),
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        // No `--token`, so the only token source is each repo's config override.
        let ctx = Context::new(config, ContextOptions::default());

        let repos = resolve_github_repos(&ctx).expect("resolve");
        let app = repos
            .iter()
            .find(|t| t.owner == "acme" && t.name == "app")
            .expect("token-less github block still yields a plan entry");
        assert!(
            app.token.is_none(),
            "no token configured for acme/app should leave the plan entry untokened"
        );
        let other = repos
            .iter()
            .find(|t| t.owner == "acme" && t.name == "other")
            .expect("second github repo resolved");
        assert_eq!(other.token.as_deref(), Some("ghp_explicit"));
    }

    #[test]
    fn resolve_github_repos_adopts_later_crates_token_for_shared_repo() {
        use anodizer_core::config::ReleaseConfig;
        use anodizer_core::config::{Config, CrateConfig, ScmRepoConfig, WorkspaceConfig};
        use anodizer_core::context::{Context, ContextOptions};

        fn github_crate(name: &str, owner: &str, repo: &str, token: Option<&str>) -> CrateConfig {
            CrateConfig {
                name: name.to_string(),
                path: ".".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: owner.to_string(),
                        name: repo.to_string(),
                        token: token.map(String::from),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        let config = Config {
            project_name: "ws".to_string(),
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![
                    // First occurrence of acme/app is UNTOKENED …
                    github_crate("a", "acme", "app", None),
                    // … a later crate names the SAME repo WITH a token — dedup must
                    // adopt it rather than discard it (else preflight fails).
                    github_crate("b", "acme", "app", Some("ghp_late")),
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = Context::new(config, ContextOptions::default());

        let repos = resolve_github_repos(&ctx).expect("resolve");
        let app = repos
            .iter()
            .find(|t| t.owner == "acme" && t.name == "app")
            .expect("shared repo resolved");
        assert_eq!(
            app.token.as_deref(),
            Some("ghp_late"),
            "the later crate's token must be adopted for the shared repo"
        );
        assert_eq!(repos.len(), 1, "the shared repo must be deduplicated");
    }

    #[test]
    fn recorded_tag_reads_github_release_evidence() {
        use anodizer_core::publish_evidence::{GithubReleaseExtra, GithubReleaseTargetSnapshot};
        use anodizer_core::{
            PublishEvidence, PublishEvidenceExtra, PublisherGroup, PublisherOutcome,
            PublisherResult,
        };

        let mut evidence = PublishEvidence::new("github-release");
        evidence.extra = PublishEvidenceExtra::GithubRelease(GithubReleaseExtra {
            github_release_targets: vec![GithubReleaseTargetSnapshot {
                crate_name: "app".into(),
                owner: "acme".into(),
                repo: "app".into(),
                tag: "v1.2.0-rc.1".into(),
                release_id: Some(42),
            }],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "github-release".into(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });

        assert_eq!(
            recorded_tag(&report, "acme", "app"),
            Some("v1.2.0-rc.1".to_string())
        );
        assert_eq!(recorded_tag(&report, "acme", "other"), None);
    }

    // --- shared builders for the resolve_github_repos branch tests ---

    fn github_crate_cfg(
        name: &str,
        owner: &str,
        repo: &str,
        token: Option<&str>,
    ) -> anodizer_core::config::CrateConfig {
        use anodizer_core::config::{CrateConfig, ReleaseConfig, ScmRepoConfig};
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: owner.to_string(),
                    name: repo.to_string(),
                    token: token.map(String::from),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A crate with a `release:` block but NO `github:` sub-block — resolves no
    /// GitHub repo, so `resolve_github_repos` must skip it.
    fn non_github_release_crate(name: &str) -> anodizer_core::config::CrateConfig {
        use anodizer_core::config::{CrateConfig, ReleaseConfig};
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }
    }

    /// A crate with no `release:` block at all — skipped by the first guard.
    fn release_less_crate(name: &str) -> anodizer_core::config::CrateConfig {
        use anodizer_core::config::CrateConfig;
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            release: None,
            ..Default::default()
        }
    }

    fn ctx_from_crates(
        crates: Vec<anodizer_core::config::CrateConfig>,
        cli_token: Option<&str>,
    ) -> Context {
        use anodizer_core::config::{Config, WorkspaceConfig};
        use anodizer_core::context::ContextOptions;
        let config = Config {
            project_name: "ws".to_string(),
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates,
                ..Default::default()
            }]),
            ..Default::default()
        };
        let options = ContextOptions {
            token: cli_token.map(String::from),
            ..Default::default()
        };
        Context::new(config, options)
    }

    #[test]
    fn resolve_github_repos_skips_release_less_and_non_github_crates() {
        // Only the crate carrying a `release.github` block yields a target;
        // the release-less crate hits the first guard, and the crate whose
        // `release:` block lacks a `github:` sub-block hits the second.
        let ctx = ctx_from_crates(
            vec![
                release_less_crate("a"),
                non_github_release_crate("b"),
                github_crate_cfg("c", "acme", "app", Some("ghp_c")),
            ],
            None,
        );
        let repos = resolve_github_repos(&ctx).expect("resolve");
        assert_eq!(repos.len(), 1, "only the github-backed crate contributes");
        assert_eq!(repos[0].owner, "acme");
        assert_eq!(repos[0].name, "app");
        assert_eq!(repos[0].token.as_deref(), Some("ghp_c"));
    }

    #[test]
    fn resolve_github_repos_keeps_first_token_when_later_crate_is_untokened() {
        // The FIRST occurrence carries the token; a later crate names the same
        // repo untokened. Dedup must NOT clobber the existing token with None.
        let ctx = ctx_from_crates(
            vec![
                github_crate_cfg("a", "acme", "app", Some("ghp_first")),
                github_crate_cfg("b", "acme", "app", None),
            ],
            None,
        );
        let repos = resolve_github_repos(&ctx).expect("resolve");
        assert_eq!(repos.len(), 1, "shared repo is deduplicated");
        assert_eq!(
            repos[0].token.as_deref(),
            Some("ghp_first"),
            "the first crate's token survives a later untokened duplicate"
        );
    }

    #[test]
    fn resolve_github_repos_adopts_cli_token_when_config_is_untokened() {
        // No per-repo and no release-stage token, but a pipeline `--token` is
        // set: resolve_token's final `.or_else(ctx.options.token)` rung adopts it.
        let ctx = ctx_from_crates(
            vec![github_crate_cfg("a", "acme", "app", None)],
            Some("ghp_cli"),
        );
        let repos = resolve_github_repos(&ctx).expect("resolve");
        assert_eq!(repos.len(), 1);
        assert_eq!(
            repos[0].token.as_deref(),
            Some("ghp_cli"),
            "the pipeline --token backfills an otherwise-untokened target"
        );
    }

    #[test]
    fn resolve_github_repos_yields_distinct_repos_in_config_order() {
        // Two DIFFERENT repos → two targets, order preserved, each tokened.
        let ctx = ctx_from_crates(
            vec![
                github_crate_cfg("a", "acme", "app", Some("ghp_a")),
                github_crate_cfg("b", "acme", "other", Some("ghp_b")),
            ],
            None,
        );
        let repos = resolve_github_repos(&ctx).expect("resolve");
        assert_eq!(repos.len(), 2, "two distinct repos are both kept");
        assert_eq!(
            (repos[0].owner.as_str(), repos[0].name.as_str()),
            ("acme", "app")
        );
        assert_eq!(repos[0].token.as_deref(), Some("ghp_a"));
        assert_eq!(
            (repos[1].owner.as_str(), repos[1].name.as_str()),
            ("acme", "other")
        );
        assert_eq!(repos[1].token.as_deref(), Some("ghp_b"));
    }

    #[test]
    fn recorded_tag_skips_wrong_name_missing_evidence_and_wrong_variant() {
        use anodizer_core::publish_evidence::{GithubReleaseExtra, GithubReleaseTargetSnapshot};
        use anodizer_core::{
            PublishEvidence, PublishEvidenceExtra, PublisherGroup, PublisherOutcome,
            PublisherResult,
        };

        fn result(name: &str, evidence: Option<PublishEvidence>) -> PublisherResult {
            PublisherResult {
                name: name.into(),
                group: PublisherGroup::Submitter,
                required: true,
                outcome: PublisherOutcome::Succeeded,
                evidence,
            }
        }

        // A non-github-release result carrying github-release-shaped evidence
        // must be ignored by the name filter.
        let mut npm_ev = PublishEvidence::new("npm");
        npm_ev.extra = PublishEvidenceExtra::GithubRelease(GithubReleaseExtra {
            github_release_targets: vec![GithubReleaseTargetSnapshot {
                crate_name: "app".into(),
                owner: "acme".into(),
                repo: "app".into(),
                tag: "v9.9.9".into(),
                release_id: Some(1),
            }],
        });

        // A github-release result whose extra is the default `Empty` variant
        // hits the `_ => None` arm.
        let mut empty_ev = PublishEvidence::new("github-release");
        empty_ev.extra = PublishEvidenceExtra::Empty;

        // The one real hit — carries MULTIPLE targets so `flatten().find(...)`
        // must pick the owner/repo match out of several.
        let mut real_ev = PublishEvidence::new("github-release");
        real_ev.extra = PublishEvidenceExtra::GithubRelease(GithubReleaseExtra {
            github_release_targets: vec![
                GithubReleaseTargetSnapshot {
                    crate_name: "core".into(),
                    owner: "acme".into(),
                    repo: "core".into(),
                    tag: "v2.0.0-rc.2".into(),
                    release_id: Some(7),
                },
                GithubReleaseTargetSnapshot {
                    crate_name: "app".into(),
                    owner: "acme".into(),
                    repo: "app".into(),
                    tag: "v2.0.0-rc.1".into(),
                    release_id: Some(8),
                },
            ],
        });

        let mut report = PublishReport::default();
        report.results.push(result("npm", Some(npm_ev)));
        // A github-release result with NO evidence exercises the evidence filter.
        report.results.push(result("github-release", None));
        report
            .results
            .push(result("github-release", Some(empty_ev)));
        report.results.push(result("github-release", Some(real_ev)));

        // Picks the matching target's tag out of the multi-target evidence,
        // ignoring the npm result, the evidence-less result, and the Empty one.
        assert_eq!(
            recorded_tag(&report, "acme", "app"),
            Some("v2.0.0-rc.1".to_string())
        );
        // The sibling target in the same evidence resolves independently.
        assert_eq!(
            recorded_tag(&report, "acme", "core"),
            Some("v2.0.0-rc.2".to_string())
        );
        // The npm result's github-shaped target must NOT leak through.
        assert_eq!(recorded_tag(&report, "acme", "nomatch"), None);
    }
}
