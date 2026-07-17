//! [`anodizer_core::Publisher`] wrapper around [`ReleaseStage::run`] for
//! the GitHub backend.
//!
//! Lives in `stage-release` (not `stage-publish`) so the release-creation
//! path can implement the trait without dragging `stage-publish` into the
//! dependency graph. `stage-publish`'s registry adds `anodizer-stage-release`
//! as a dep and pushes [`GithubReleasePublisher`] into the configured
//! publisher list when [`ctx.token_type`](anodizer_core::context::Context)
//! is GitHub.
//!
//! Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable bytes,
//! server-side deletable). `required = true` — failure to create the
//! GitHub release fails the overall publish run (everything downstream
//! that resolves download URLs against the release URL would otherwise
//! publish broken manifests).
//!
//! # Rollback shape
//!
//! One server-side operation per recorded target:
//! `DELETE /repos/{owner}/{repo}/releases/{id}` — removes the release
//! and every attached asset.
//!
//! The tag ref is deliberately NOT touched here: tag lifecycle (create,
//! push, delete) has a single owner — `anodizer tag` / `anodizer tag
//! rollback` (which the release failure policy drives) — so publisher
//! rollback and tag rollback can never race a double-delete, and
//! operator `on_rollback` hooks never observe a half-deleted
//! release/tag pair.
//!
//! A 404 response buckets as [`ReleaseDeleteOutcome::AlreadyAbsent`]
//! so re-running `--rollback-only` after a partial success does not
//! surface false failures.
//!
//! # ID capture
//!
//! [`crate::run::ReleaseStage`]'s body is unchanged per the
//! release-resilience contract. To learn each release's numeric ID
//! (required for `DELETE /releases/{id}`) the publisher queries
//! [`anodizer_core::github_client::GitHubClient::get_release_by_tag`]
//! once per configured (owner, repo, tag) target after `ReleaseStage::run`
//! returns. The same client is reused for both delete operations during
//! rollback. Tests inject a [`MockGitHubClient`](anodizer_core::github_client::MockGitHubClient)
//! via [`GithubReleasePublisher::with_client`].
//!
//! # Credential handling
//!
//! [`GithubReleaseTarget`] stores `(owner, repo, tag, release_id)` only.
//! Auth tokens are resolved from the live process environment at
//! `run` / `rollback` time and never persisted into evidence —
//! `dist/run-<id>/report.json` and the announce-time release-body
//! summary carry zero secret material.

use std::collections::HashMap;
use std::sync::Arc;

use anodizer_core::config::ScmRepoConfig;
use anodizer_core::context::Context;
use anodizer_core::github_client::{DeleteReleaseParams, GetReleaseByTagParams, GitHubClient};
use anodizer_core::scm::ScmTokenType;
use anodizer_core::stage::Stage;

use crate::ReleaseStage;

/// Bounded fan-out cap for the rollback delete loop. Mirrors the
/// `ROLLBACK_PARALLELISM` constant used by `stage-publish`'s git-revert
/// and close-PR publishers; kept inline rather than re-exported so
/// `stage-release` does not depend on `stage-publish`.
const ROLLBACK_PARALLELISM: usize = 4;

// ---------------------------------------------------------------------------
// GithubReleaseTarget — evidence shape
// ---------------------------------------------------------------------------

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`]. Captures the (owner, repo,
/// tag) coordinates the publish path acted on plus the numeric
/// release ID (when GitHub returned one). The release ID is `None`
/// when the post-publish `get_release_by_tag` lookup failed —
/// rollback for that row skips the release-delete step (there is
/// nothing else to do here; the tag ref is owned by tag rollback).
pub(crate) type GithubReleaseTarget = anodizer_core::publish_evidence::GithubReleaseTargetSnapshot;

/// Decode the GithubRelease variant from
/// [`anodizer_core::PublishEvidence::extra`]. Returns an empty Vec
/// when the variant doesn't match.
fn decode_github_release_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<GithubReleaseTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::GithubRelease(g) => g.github_release_targets.clone(),
        _ => Vec::new(),
    }
}

/// Three-bucket outcome for a single release DELETE call.
/// `AlreadyAbsent` is a success bucket — re-running rollback
/// after a partial success must NOT surface 404s as failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseDeleteOutcome {
    Deleted,
    AlreadyAbsent,
    Failed(String),
}

/// Classify an `anyhow::Error` from a [`GitHubClient`] delete call into
/// the three rollback outcome buckets. Substring-matches the lowercased
/// error message against the shapes GitHub returns when the target is
/// already gone:
///
/// - `404` / `not found` — the canonical "release or tag-ref does not
///   exist" response.
/// - `410` / `gone` — GitHub occasionally returns 410 for resources that
///   were deleted recently (the target was "tombstoned").
/// - `422` / `unprocessable` — GitHub's "does not exist" edge cases on
///   recently-mutated resources surface as 422 Unprocessable Entity.
///
/// Every other error message buckets as `Failed` so genuine transport /
/// auth / 5xx failures still surface a manual-cleanup warn.
fn classify_delete_err(err: &anyhow::Error) -> ReleaseDeleteOutcome {
    let s = err.to_string().to_ascii_lowercase();
    let already_absent = s.contains("404")
        || s.contains("not found")
        || s.contains("410")
        || s.contains("gone")
        || s.contains("422")
        || s.contains("unprocessable");
    if already_absent {
        ReleaseDeleteOutcome::AlreadyAbsent
    } else {
        ReleaseDeleteOutcome::Failed(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// GithubReleasePublisher
// ---------------------------------------------------------------------------

/// [`anodizer_core::Publisher`] adapter over [`ReleaseStage::run`]'s
/// GitHub backend. See module rustdoc for the design.
pub struct GithubReleasePublisher {
    client: Arc<dyn GitHubClient + Send + Sync>,
    required_override: Option<bool>,
    retain_on_rollback_override: Option<bool>,
}

impl GithubReleasePublisher {
    /// Stable lowercase publisher identifier.
    pub const PUBLISHER_NAME: &'static str = "github-release";
    /// Scheduling group.
    pub const PUBLISHER_GROUP: anodizer_core::PublisherGroup =
        anodizer_core::PublisherGroup::Assets;
    /// Built-in default: required — failure fails the overall release.
    pub const PUBLISHER_REQUIRED: bool = true;
    /// OAuth / token scope rollback needs.
    pub const ROLLBACK_SCOPE: Option<&'static str> = Some("GITHUB_TOKEN contents:write");

    /// Construct with a production [`gh`]-CLI-backed GitHub client.
    ///
    /// [`gh`]: https://cli.github.com
    pub fn new() -> Self {
        Self {
            client: Arc::new(gh_cli_client::GhCliGitHubClient),
            required_override: None,
            retain_on_rollback_override: None,
        }
    }

    /// Construct with a config-supplied `required` override.
    ///
    /// Pass the `Option<bool>` read from the release config. `None` keeps
    /// the built-in default (`true`); `Some(v)` overrides it for this run.
    pub fn with_required(required_override: Option<bool>) -> Self {
        Self {
            client: Arc::new(gh_cli_client::GhCliGitHubClient),
            required_override,
            retain_on_rollback_override: None,
        }
    }

    /// Construct with config-supplied `required` and `retain_on_rollback` overrides.
    pub fn with_overrides(
        required_override: Option<bool>,
        retain_on_rollback_override: Option<bool>,
    ) -> Self {
        Self {
            client: Arc::new(gh_cli_client::GhCliGitHubClient),
            required_override,
            retain_on_rollback_override,
        }
    }

    /// Construct with a caller-provided client. Used by tests to inject a
    /// [`MockGitHubClient`](anodizer_core::github_client::MockGitHubClient).
    pub fn with_client(client: Arc<dyn GitHubClient + Send + Sync>) -> Self {
        Self {
            client,
            required_override: None,
            retain_on_rollback_override: None,
        }
    }
}

impl Default for GithubReleasePublisher {
    fn default() -> Self {
        Self::new()
    }
}

/// Crates in scope for the current `--crate` selection whose `release`
/// block is configured and not skip-inactive. Empty selection = every
/// crate; non-empty = exactly those names — matching
/// [`Publisher::config_fully_inactive`]'s chokepoint semantics so a
/// `--crate` selection can never mask a skipped release as active via an
/// out-of-scope sibling. Shared by target collection, the preflight probe,
/// and `config_fully_inactive` so the three cannot drift.
///
/// [`Publisher::config_fully_inactive`]: anodizer_core::Publisher::config_fully_inactive
fn active_release_configs(ctx: &Context) -> Vec<&anodizer_core::config::CrateConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| {
            let Some(release_cfg) = c.release.as_ref() else {
                return false;
            };
            !anodizer_core::env_preflight::entry_inactive(
                ctx,
                release_cfg.skip.as_ref(),
                None,
                None,
            )
        })
        .collect()
}

/// Walk the crate universe (top-level `crates` plus every
/// `workspaces[].crates` entry) and emit one [`GithubReleaseTarget`] per
/// crate that has a `release.github` block (or falls back to the
/// `release.github` default per [`crate::resolve_release_repo`]).
/// `release_id` is left `None`; the post-publish lookup fills it in.
///
/// Crates whose `release.skip` would evaluate true are skipped here too,
/// matching the publish path's filter. Render errors are surfaced so
/// the caller fails loudly rather than silently dropping a target.
fn collect_release_targets(ctx: &Context) -> anyhow::Result<Vec<GithubReleaseTarget>> {
    use crate::release_body::resolve_release_tag;
    use crate::resolve_release_repo;

    let mut out: Vec<GithubReleaseTarget> = Vec::new();
    for c in active_release_configs(ctx) {
        let release_cfg = c
            .release
            .as_ref()
            .expect("filtered by active_release_configs");
        let Some(ScmRepoConfig { owner, name, .. }) =
            resolve_release_repo(release_cfg, ScmTokenType::GitHub, ctx)?
        else {
            continue;
        };
        let tag = resolve_release_tag(
            ctx,
            c.resolved_tag_template(),
            release_cfg.tag.as_deref(),
            &c.name,
        )?;
        out.push(GithubReleaseTarget {
            crate_name: c.name.clone(),
            owner,
            repo: name,
            tag,
            release_id: None,
        });
    }
    Ok(out)
}

/// Resolve each target's numeric release ID via
/// [`GitHubClient::get_release_by_tag`], memoized by `(owner, repo, tag)`.
///
/// Workspaces where multiple crates share a single tag (the common
/// monorepo shape — one workspace-wide tag pointing at one GitHub
/// release) would otherwise N-query the GitHub API for the same release.
/// The memo collapses every duplicate tuple to one round-trip and reuses
/// the cached `Option<u64>` for the rest.
///
/// Transport / auth failures are swallowed: the publish itself already
/// succeeded; failing the run because a post-publish enrichment 5xx'd
/// would lose the (owner, repo, tag) evidence rollback still needs.
fn capture_release_ids(
    client: &(dyn GitHubClient + Send + Sync),
    targets: &mut [GithubReleaseTarget],
) {
    let mut memo: HashMap<(String, String, String), Option<u64>> = HashMap::new();
    for target in targets.iter_mut() {
        let key = (
            target.owner.clone(),
            target.repo.clone(),
            target.tag.clone(),
        );
        if let Some(cached) = memo.get(&key) {
            target.release_id = *cached;
            continue;
        }
        let params = GetReleaseByTagParams {
            owner: target.owner.clone(),
            repo: target.repo.clone(),
            tag: target.tag.clone(),
        };
        let resolved = match client.get_release_by_tag(&params) {
            Ok(Some(info)) => Some(info.id),
            // Tag has no release — leave id as None; rollback will skip
            // the release-delete step for this row.
            Ok(None) => None,
            // Transport / auth failure looking up the ID. Don't fail
            // the publish over a post-publish enrichment; leave id as
            // None so rollback skips the release-delete for this row.
            Err(_e) => None,
        };
        target.release_id = resolved;
        memo.insert(key, resolved);
    }
}

// ---------------------------------------------------------------------------
// Pre-tag probe — github-release write-access gate
// ---------------------------------------------------------------------------

/// Keep the most severe of two outcomes: `Blocker` > `Warning` > `Pass`. The
/// first-seen message wins within a severity so the operator sees a stable line.
fn preflight_merge(
    acc: anodizer_core::PreflightCheck,
    next: anodizer_core::PreflightCheck,
) -> anodizer_core::PreflightCheck {
    acc.merge(next)
}

/// (owner, repo) per selected, non-skipped crate carrying a `release.github`
/// (or default release) block. Mirrors [`collect_release_targets`] minus tag
/// resolution — the probe needs only the repo coordinates and must not depend
/// on a version being stamped yet (preflight runs before the tag).
fn release_repo_targets_for_preflight(ctx: &Context) -> Vec<(String, String)> {
    use crate::resolve_release_repo;
    let mut out = Vec::new();
    for c in active_release_configs(ctx) {
        let release_cfg = c
            .release
            .as_ref()
            .expect("filtered by active_release_configs");
        if let Ok(Some(ScmRepoConfig { owner, name, .. })) =
            resolve_release_repo(release_cfg, ScmTokenType::GitHub, ctx)
        {
            out.push((owner, name));
        }
    }
    out
}

/// Live pre-tag gate for the REQUIRED github-release publisher: prove the
/// resolved GitHub token can create a release (push access) in every
/// configured release repo. Pass (no network) when no release is
/// configured/selected; Blocker when the token is missing or a target repo is
/// unwritable; Warning on a transient rate-limit / transport blip.
fn github_release_preflight(ctx: &Context) -> anodizer_core::PreflightCheck {
    use anodizer_core::PreflightCheck;
    let targets = release_repo_targets_for_preflight(ctx);
    if targets.is_empty() {
        return PreflightCheck::Pass;
    }
    let token =
        anodizer_core::git::resolve_github_token_with_env(ctx.options.token.as_deref(), &|k| {
            ctx.env_var(k)
        });
    let Some(token) = token else {
        let (owner, repo) = &targets[0];
        return PreflightCheck::Blocker(format!(
            "no GitHub token resolved ({}); cannot create the release in {owner}/{repo}",
            anodizer_core::git::github_token_hint()
        ));
    };
    let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut acc = PreflightCheck::Pass;
    for (owner, repo) in targets {
        if !seen.insert((owner.clone(), repo.clone())) {
            continue;
        }
        acc = preflight_merge(
            acc,
            repo_push_check(
                &owner,
                &repo,
                &token,
                &policy,
                ctx.config.github_urls.as_ref(),
                ctx.preflight_is_strict(),
                ctx.env_source(),
                &ctx.logger("preflight"),
            ),
        );
    }
    acc
}

/// Probe `GET {api_base}/repos/{owner}/{repo}` for push access, resolving
/// the base through [`anodizer_core::http::github_api_base_with_config`] —
/// the probe must contact the same host the release backend will
/// (`github_urls.api` on GHES), not hardcoded github.com.
/// See [`repo_push_check_at`] for the status/permission mapping.
#[allow(clippy::too_many_arguments)]
fn repo_push_check<E: anodizer_core::EnvSource + ?Sized>(
    owner: &str,
    repo: &str,
    token: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    github_urls: Option<&anodizer_core::config::GitHubUrlsConfig>,
    strict: bool,
    env: &E,
    log: &anodizer_core::log::StageLogger,
) -> anodizer_core::PreflightCheck {
    let base = anodizer_core::http::github_api_base_with_config(github_urls, env);
    let url = format!("{base}/repos/{owner}/{repo}");
    repo_push_check_at(&url, owner, repo, token, policy, strict, log)
}

/// `url`-taking core of [`repo_push_check`] so a unit test can drive the
/// status/permission mapping against a local responder.
///
/// github-release is REQUIRED and creates the release + uploads assets, so a
/// definitively-unwritable target is a hard prerequisite the run cannot
/// satisfy — those map to Blocker. A transient rate-limit / transport blip must
/// not abort a release that would otherwise succeed — those warn.
///
/// * 200 + `permissions.push == true` ⇒ Pass
/// * 200 + `permissions.push == false` ⇒ Blocker (token lacks `contents:write`)
/// * 200 + `permissions` absent ⇒ Warning (push scope undeterminable;
///   Blocker under strict preflight)
/// * 404 / 401 / 403 without a rate-limit signal ⇒ Blocker
/// * 429, or 401 / 403 carrying a rate-limit header ⇒ Warning (Blocker under
///   strict preflight)
/// * 5xx / transport failure / unexpected status ⇒ Warning (Blocker under
///   strict preflight)
fn repo_push_check_at(
    url: &str,
    owner: &str,
    repo: &str,
    token: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    strict: bool,
    log: &anodizer_core::log::StageLogger,
) -> anodizer_core::PreflightCheck {
    use anodizer_core::PreflightCheck;
    anodizer_core::git::github_repo_push_check(
        url,
        owner,
        repo,
        Some(token),
        policy,
        anodizer_core::git::RepoAccessOutcomes {
            push_denied: PreflightCheck::Blocker(format!(
                "GitHub token cannot push to {owner}/{repo} (needs contents:write); \
                 the release create / asset upload will fail"
            )),
            // A missing repo or a token that cannot access it is a hard
            // prerequisite the release-create path cannot satisfy — block.
            missing_or_denied: PreflightCheck::Blocker(format!(
                "release repo {owner}/{repo} not found or the GitHub token lacks access; \
                 cannot create the release"
            )),
        },
        strict,
        log,
    )
}

impl anodizer_core::Publisher for GithubReleasePublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        self.required_override.unwrap_or(Self::PUBLISHER_REQUIRED)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_release_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // GitHub release creation + asset upload authenticate via the
        // same ladder the stage uses: explicit `--token` option, else
        // ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN. Self-gates on a `release:`
        // block existing anywhere in the crate universe so ungated
        // requirement collection never demands a token from a config that
        // creates no release.
        let configured = ctx
            .config
            .crate_universe()
            .into_iter()
            .any(|c| c.release.is_some());
        if !configured || ctx.options.token.as_deref().is_some_and(|t| !t.is_empty()) {
            return Vec::new();
        }
        vec![anodizer_core::EnvRequirement::EnvAnyOf {
            vars: anodizer_core::git::GITHUB_TOKEN_ENV_LADDER
                .iter()
                .map(|v| v.to_string())
                .collect(),
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        // Existing ReleaseStage::run body is unchanged per the
        // release-resilience contract. We delegate to it for the
        // publish itself, then enumerate (owner, repo, tag) targets
        // from config and ask GitHub for each release's numeric ID.
        <ReleaseStage as Stage>::run(&ReleaseStage, ctx)?;

        let mut targets = collect_release_targets(ctx)?;
        // Skip ID capture in dry-run / snapshot — no release was created
        // so `get_release_by_tag` would 404 (or worse, retry-loop on
        // transport errors). Evidence still captures the (owner, repo,
        // tag) tuples so the rollback report names what was targeted.
        if !ctx.is_dry_run() && !ctx.is_snapshot() {
            capture_release_ids(self.client.as_ref(), &mut targets);
        }

        let mut evidence = anodizer_core::PublishEvidence::new(Self::PUBLISHER_NAME);
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://github.com/{}/{}/releases/tag/{}",
                first.owner, first.repo, first.tag
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: targets,
            },
        );
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_github_release_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&anodizer_core::rollback_empty_warning_msg(
                Self::PUBLISHER_NAME,
                "release targets",
            ));
            return Ok(());
        }

        // The release-delete only fires when `release_id` was captured.
        // The tag ref is owned by `anodizer tag rollback` (see module
        // docs) and is never deleted from this path.
        let mut release_deleted = 0usize;
        let mut release_already_absent = 0usize;
        let mut release_failed = 0usize;

        for chunk in targets.chunks(ROLLBACK_PARALLELISM) {
            // Synchronous per-chunk fan-out via `std::thread::scope` —
            // mirrors krew's rollback shape and avoids pulling tokio
            // into this code path. The chunk size cap keeps GitHub's
            // secondary-rate-limit window comfortable.
            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(chunk.len());
                for target in chunk {
                    let client = Arc::clone(&self.client);
                    let log = log.clone();
                    handles.push(s.spawn(move || {
                        let release_outcome = if let Some(id) = target.release_id {
                            log.status(&format!(
                                "deleting {} release {} (id={}) from {}/{}",
                                GithubReleasePublisher::PUBLISHER_NAME,
                                target.tag,
                                id,
                                target.owner,
                                target.repo
                            ));
                            let params = DeleteReleaseParams {
                                owner: target.owner.clone(),
                                repo: target.repo.clone(),
                                release_id: id,
                            };
                            match client.delete_release(&params) {
                                Ok(()) => ReleaseDeleteOutcome::Deleted,
                                Err(e) => classify_delete_err(&e),
                            }
                        } else {
                            // No captured release_id — skip the release
                            // delete (it would 404 anyway). Treat as
                            // already-absent for the counter.
                            log.status(&format!(
                                "skipped release delete — no captured {} release id for {} \
                                 on {}/{}",
                                GithubReleasePublisher::PUBLISHER_NAME,
                                target.tag,
                                target.owner,
                                target.repo,
                            ));
                            ReleaseDeleteOutcome::AlreadyAbsent
                        };

                        // Surface failure warns with the same wording
                        // shape every other publisher uses so an operator
                        // skimming the rollback log can pattern-match.
                        if let ReleaseDeleteOutcome::Failed(err) = &release_outcome {
                            log.warn(&rollback_failure_msg(
                                "release",
                                &target.tag,
                                &target.owner,
                                &target.repo,
                                err,
                            ));
                        }
                        release_outcome
                    }));
                }
                for h in handles {
                    // A panicked worker must not abort the rollback summary —
                    // one crashed delete would otherwise hide the counters
                    // for every sibling target. Translate the panic into a
                    // Failed outcome so the operator still sees the
                    // per-target failure in the summary line below.
                    let r = match anodizer_core::parallel::join_panic_to_err(
                        h.join(),
                        "github-release rollback",
                    ) {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            log.warn(&format!("{err}"));
                            ReleaseDeleteOutcome::Failed(format!("{err}"))
                        }
                    };
                    match r {
                        ReleaseDeleteOutcome::Deleted => release_deleted += 1,
                        ReleaseDeleteOutcome::AlreadyAbsent => release_already_absent += 1,
                        ReleaseDeleteOutcome::Failed(_) => release_failed += 1,
                    }
                }
            });
        }

        log.status(&format!(
            "{} rollback deleted {} release(s), {} already-absent, {} failed \
             (tag refs untouched — tag rollback owns them)",
            Self::PUBLISHER_NAME,
            release_deleted,
            release_already_absent,
            release_failed,
        ));
        Ok(())
    }

    /// Live pre-tag gate. github-release is REQUIRED and creates the release +
    /// uploads assets; a token that is missing, invalid, or lacks
    /// `contents:write` to a configured release repo would fail release
    /// creation AFTER one-way-door publishers (cargo / chocolatey / …) may have
    /// already fired. Probe every configured release repo for push access
    /// BEFORE the tag is cut. Pass (no network) when no release is
    /// configured/selected. See [`github_release_preflight`].
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(github_release_preflight(ctx))
    }

    fn skips_on_nightly(&self) -> bool {
        // GitHub Releases accepts overwrites; nightly re-cuts are the primary
        // use-case for keep_single_release, so nightly runs are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        self.retain_on_rollback_override.unwrap_or(false)
    }
}

/// Canonical wording for a per-target rollback failure warn line. Keeps
/// the shape consistent with sibling publishers'
/// `rollback_failure_warning_msg` in `stage-publish::publisher_helpers`
/// without reaching across the crate boundary.
fn rollback_failure_msg(step: &str, tag: &str, owner: &str, repo: &str, err: &str) -> String {
    format!(
        "github-release {step} delete failed for {tag} on {owner}/{repo}: {err}; \
         manual cleanup required at https://github.com/{owner}/{repo}/releases/tag/{tag}; \
         check {} is set in this shell",
        anodizer_core::git::github_token_env_hint()
    )
}

// ---------------------------------------------------------------------------
// gh_cli_client — production GitHubClient impl backed by the `gh` CLI
// ---------------------------------------------------------------------------

mod gh_cli_client {
    //! Minimal `gh` CLI / reqwest backed [`GitHubClient`] used by
    //! [`super::GithubReleasePublisher`] for ID lookup + delete.
    //!
    //! The publisher only consumes `get_release_by_tag`, `delete_release`,
    //! and `delete_tag`; the remaining trait methods return an explicit
    //! "not implemented" error so a future call site that wires this
    //! client into a wider code path fails loudly rather than silently
    //! no-opping. The real production publish path (release create +
    //! asset upload) still goes through the octocrab client in
    //! [`crate::github`].

    use anodizer_core::github_client::{
        AssetInfo, CreateReleaseParams, DeleteReleaseParams, DeleteTagParams,
        GetReleaseByTagParams, GitHubClient, ListReleasesParams, ReleaseInfo, UploadAssetParams,
    };

    pub(crate) struct GhCliGitHubClient;

    impl GitHubClient for GhCliGitHubClient {
        fn create_release(&self, _params: &CreateReleaseParams) -> anyhow::Result<ReleaseInfo> {
            anyhow::bail!("GhCliGitHubClient: create_release not implemented (use octocrab path)")
        }
        fn upload_asset(&self, _params: &UploadAssetParams) -> anyhow::Result<AssetInfo> {
            anyhow::bail!("GhCliGitHubClient: upload_asset not implemented (use octocrab path)")
        }
        fn list_releases(&self, _params: &ListReleasesParams) -> anyhow::Result<Vec<ReleaseInfo>> {
            anyhow::bail!("GhCliGitHubClient: list_releases not implemented (use octocrab path)")
        }

        fn get_release_by_tag(
            &self,
            params: &GetReleaseByTagParams,
        ) -> anyhow::Result<Option<ReleaseInfo>> {
            let endpoint = format!(
                "/repos/{}/{}/releases/tags/{}",
                params.owner, params.repo, params.tag
            );
            match anodizer_core::git::gh_api_get(&endpoint, None) {
                Ok(v) => {
                    // Successful 200 — extract the minimal fields. A
                    // 404 surfaces as an Err with "404" in the message,
                    // handled in the Err arm below.
                    let id = v["id"].as_u64().ok_or_else(|| {
                        anyhow::anyhow!(
                            "GhCliGitHubClient: get_release_by_tag response missing 'id' field"
                        )
                    })?;
                    let html_url = v["html_url"].as_str().unwrap_or("").to_string();
                    let tag_name = v["tag_name"].as_str().unwrap_or(&params.tag).to_string();
                    let name = v["name"].as_str().map(str::to_string);
                    let draft = v["draft"].as_bool().unwrap_or(false);
                    Ok(Some(ReleaseInfo {
                        id,
                        html_url,
                        tag_name,
                        name,
                        draft,
                    }))
                }
                Err(e) => {
                    let s = e.to_string().to_ascii_lowercase();
                    if s.contains("404") || s.contains("not found") {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                }
            }
        }

        fn delete_release(&self, params: &DeleteReleaseParams) -> anyhow::Result<()> {
            let endpoint = format!(
                "/repos/{}/{}/releases/{}",
                params.owner, params.repo, params.release_id
            );
            gh_api_delete(&endpoint)
        }

        fn delete_tag(&self, params: &DeleteTagParams) -> anyhow::Result<()> {
            let endpoint = format!(
                "/repos/{}/{}/git/refs/tags/{}",
                params.owner, params.repo, params.tag
            );
            gh_api_delete(&endpoint)
        }
    }

    /// `gh api --method DELETE <endpoint>` returning `Ok(())` on 2xx,
    /// `Err(_)` otherwise. The error string preserves "404 Not Found"
    /// so the caller can bucket it via substring match.
    fn gh_api_delete(endpoint: &str) -> anyhow::Result<()> {
        use anyhow::Context as _;
        use std::process::Command;
        let output = Command::new("gh")
            .args(["api", "--method", "DELETE", endpoint])
            .output()
            .context("failed to spawn gh CLI")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        anyhow::bail!("gh api DELETE {} failed: {}", endpoint, stderr.trim())
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;

    fn tlog() -> &'static anodizer_core::log::StageLogger {
        anodizer_core::test_helpers::test_logger()
    }
    use anodizer_core::config::{CrateConfig, ReleaseConfig, ScmRepoConfig, StringOrBool};
    use anodizer_core::github_client::MockGitHubClient;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn github_release_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                    token: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The remediation hint must name the token ladder in real resolution
    /// order (ANODIZER_GITHUB_TOKEN first), rendered from
    /// GITHUB_TOKEN_ENV_LADDER — a hand-spelled hint reversed it.
    #[test]
    fn rollback_failure_msg_names_token_ladder_in_precedence_order() {
        let msg = rollback_failure_msg("release", "v1.0.0", "acme", "widget", "boom");
        assert!(
            msg.contains("github-release release delete failed"),
            "{msg}"
        );
        assert!(
            msg.contains("ANODIZER_GITHUB_TOKEN or GITHUB_TOKEN or GH_TOKEN"),
            "{msg}"
        );
        assert!(msg.contains("manual cleanup required"), "{msg}");
    }

    #[test]
    fn github_release_publisher_classification() {
        let p = GithubReleasePublisher::new();
        assert_eq!(p.name(), "github-release");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn config_fully_inactive_true_when_release_skip_true_on_every_crate() {
        let mut skipped = github_release_crate("x");
        skipped.release.as_mut().unwrap().skip = Some(StringOrBool::Bool(true));
        let ctx = TestContextBuilder::new().crates(vec![skipped]).build();
        assert!(
            GithubReleasePublisher::new().config_fully_inactive(&ctx),
            "every crate's release.skip is true; the publisher must not report Succeeded for a release it never created"
        );
    }

    #[test]
    fn config_fully_inactive_true_when_selected_crate_release_skip_sibling_active() {
        let mut skipped = github_release_crate("x");
        skipped.release.as_mut().unwrap().skip = Some(StringOrBool::Bool(true));
        let ctx = TestContextBuilder::new()
            .crates(vec![skipped, github_release_crate("y")])
            .selected_crates(vec!["x".to_string()])
            .build();
        assert!(
            GithubReleasePublisher::new().config_fully_inactive(&ctx),
            "selecting a release.skip crate must not be masked as active by an out-of-scope sibling"
        );
    }

    #[test]
    fn config_fully_inactive_false_when_any_crate_release_is_active() {
        let ctx = TestContextBuilder::new()
            .crates(vec![github_release_crate("x")])
            .build();
        assert!(!GithubReleasePublisher::new().config_fully_inactive(&ctx));
    }

    #[test]
    fn config_fully_inactive_true_when_no_release_block_configured() {
        // The registry only pushes this publisher when
        // `is_github_release_configured` already found a `release:` block
        // somewhere, so this path is a defensive edge case, not a live one —
        // but it must still agree with `active_release_configs`'s empty-set
        // semantics rather than special-casing "never configured" as active.
        let ctx = TestContextBuilder::new().build();
        assert!(GithubReleasePublisher::new().config_fully_inactive(&ctx));
    }

    #[test]
    fn github_release_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = GithubReleasePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    fn one_attempt_policy() -> anodizer_core::retry::RetryPolicy {
        anodizer_core::retry::RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(1),
        }
    }

    fn http(status_line: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn repo_push_check_passes_when_push_true() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (addr, _c) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let url = format!("http://{addr}/repos/acme/widget");
        assert!(matches!(
            repo_push_check_at(
                &url,
                "acme",
                "widget",
                "tok",
                &one_attempt_policy(),
                false,
                tlog()
            ),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn repo_push_check_blocks_when_push_false() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (addr, _c) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":false}}"#)]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(
            &url,
            "acme",
            "widget",
            "tok",
            &one_attempt_policy(),
            false,
            tlog(),
        ) {
            PreflightCheck::Blocker(m) => assert!(m.contains("contents:write"), "{m}"),
            _ => panic!("expected Blocker when permissions.push is false"),
        }
    }

    #[test]
    fn repo_push_check_resolves_base_from_env_override() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // The probe must route through the shared github_api_base resolver:
        // the env override redirects it to the local responder instead of
        // the hardcoded public host.
        let (addr, calls) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let env = anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        assert!(matches!(
            repo_push_check(
                "acme",
                "widget",
                "tok",
                &one_attempt_policy(),
                None,
                false,
                &env,
                tlog()
            ),
            PreflightCheck::Pass
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must hit the env-resolved base"
        );
    }

    #[test]
    fn repo_push_check_prefers_configured_ghes_api_base() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A GHES `github_urls.api` config must win: the release backend
        // contacts that host, so the preflight verdict is only meaningful
        // against the same host.
        let (addr, calls) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let urls = anodizer_core::config::GitHubUrlsConfig {
            api: Some(format!("http://{addr}/api/v3")),
            ..Default::default()
        };
        assert!(matches!(
            repo_push_check(
                "acme",
                "widget",
                "tok",
                &one_attempt_policy(),
                Some(&urls),
                false,
                &anodizer_core::MapEnvSource::new(),
                tlog(),
            ),
            PreflightCheck::Pass
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must hit the configured GHES base"
        );
    }

    #[test]
    fn repo_push_check_blocks_on_auth_denied() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A 401 with no rate-limit header ⇒ the token cannot access the repo.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("401 Unauthorized", "")]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(
            &url,
            "acme",
            "widget",
            "bad",
            &one_attempt_policy(),
            false,
            tlog(),
        ) {
            PreflightCheck::Blocker(m) => assert!(m.contains("acme/widget"), "{m}"),
            _ => panic!("expected Blocker on a 401"),
        }
    }

    fn http_with_header(status_line: &str, header: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status_line}\r\n{header}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn repo_push_check_warns_on_rate_limited_403() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A secondary-rate-limit 403 carries `X-RateLimit-Remaining: 0`; it is
        // transient (RepoProbe::RateLimited) and must NOT block a release whose
        // token is actually valid.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http_with_header(
            "403 Forbidden",
            "X-RateLimit-Remaining: 0",
            "",
        )]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(
            &url,
            "acme",
            "widget",
            "tok",
            &one_attempt_policy(),
            false,
            tlog(),
        ) {
            PreflightCheck::Warning(m) => assert!(m.contains("rate-limited"), "{m}"),
            other => panic!("rate-limited 403 must degrade to Warning, not {other:?}"),
        }
    }

    #[test]
    fn repo_push_check_warns_on_inconclusive_5xx() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A 5xx is host-reachable-but-not-answering-cleanly
        // (RepoProbe::Inconclusive); it must warn rather than abort a release
        // whose token is fine.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("503 Service Unavailable", "")]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(
            &url,
            "acme",
            "widget",
            "tok",
            &one_attempt_policy(),
            false,
            tlog(),
        ) {
            PreflightCheck::Warning(m) => assert!(m.contains("acme/widget"), "{m}"),
            other => panic!("inconclusive 5xx must degrade to Warning, not {other:?}"),
        }
    }

    #[test]
    fn github_release_preflight_blocks_when_token_missing() {
        // A configured release repo + sealed env (no GITHUB_TOKEN) ⇒ Blocker
        // with no network call.
        let ctx = TestContextBuilder::new()
            .crates(vec![github_release_crate("demo")])
            .sealed_env()
            .build();
        let p = GithubReleasePublisher::new();
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Blocker(m) => assert!(m.contains("no GitHub token"), "{m}"),
            _ => panic!("expected Blocker when no token resolves"),
        }
    }

    #[test]
    fn github_release_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("github-release");
        let p = GithubReleasePublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("github-release")
                && m.contains("release targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn github_release_target_extra_roundtrips() {
        let original = vec![
            GithubReleaseTarget {
                crate_name: "demo".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "v1.0.0".into(),
                release_id: Some(42),
            },
            GithubReleaseTarget {
                crate_name: "demo-helper".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "helper/v0.1.0".into(),
                release_id: None,
            },
        ];
        let extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: original.clone(),
            },
        );
        let decoded = decode_github_release_targets(&extra);
        assert_eq!(decoded, original);
    }

    /// Evidence MUST NOT serialize anything that looks like a secret —
    /// no token / auth / password keys, no bearer prefixes. The shape
    /// is fully controlled by `GithubReleaseTarget`, but a future field
    /// addition could regress this contract; pin it explicitly.
    #[test]
    fn github_release_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence and assert (a) no
        // credential-shaped keys appear AND (b) the operator-public
        // (owner, repo, tag) coordinates serialize.
        let mut e = PublishEvidence::new("github-release");
        e.extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: vec![GithubReleaseTarget {
                    crate_name: "demo".into(),
                    owner: "acme".into(),
                    repo: "widget".into(),
                    tag: "v1.0.0".into(),
                    release_id: Some(42),
                }],
            },
        );
        let serialized = serde_json::to_string(&e).expect("serialize");
        let lower = serialized.to_ascii_lowercase();
        for forbidden in [
            "token",
            "auth",
            "password",
            "secret",
            "bearer",
            "credential",
            "api_key",
            "apikey",
            "private_key",
        ] {
            assert!(
                !lower.contains(forbidden),
                "evidence must not contain '{forbidden}': {serialized}"
            );
        }
        // Positive shape: (owner, repo, tag) coordinates present.
        assert!(serialized.contains("\"owner\":\"acme\""), "{serialized}");
        assert!(serialized.contains("\"repo\":\"widget\""), "{serialized}");
        assert!(serialized.contains("\"tag\":\"v1.0.0\""), "{serialized}");
    }

    #[test]
    fn github_release_rollback_treats_404_as_already_absent() {
        let mock = MockGitHubClient::new();
        // The release DELETE returns an error whose message contains
        // "404 Not Found" — the classifier should bucket it as
        // AlreadyAbsent so the rollback returns Ok and the counter
        // sums match.
        mock.set_delete_release_response(Err("HTTP 404 Not Found".to_string()));
        let mock = Arc::new(mock);
        let p = GithubReleasePublisher::with_client(mock.clone());

        let target = GithubReleaseTarget {
            crate_name: "demo".into(),
            owner: "acme".into(),
            repo: "widget".into(),
            tag: "v1.0.0".into(),
            release_id: Some(42),
        };
        let mut evidence = PublishEvidence::new("github-release");
        evidence.extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: vec![target.clone()],
            },
        );

        let mut ctx = TestContextBuilder::new().build();
        p.rollback(&mut ctx, &evidence)
            .expect("rollback returns Ok even when the delete 404s");

        // The release delete ran exactly once; classifier bucketed it as
        // AlreadyAbsent (no panic / fail-fast). The tag ref must never be
        // touched — tag rollback owns it.
        assert_eq!(mock.delete_release_call_count(), 1);
        assert_eq!(mock.delete_tag_call_count(), 0);
        let rel_calls = mock.delete_release_calls();
        assert_eq!(rel_calls[0].release_id, 42);

        // Pin the classifier shape directly so a future refactor of
        // `classify_delete_err` cannot silently widen the "AlreadyAbsent"
        // bucket.
        let err = anyhow::anyhow!("HTTP 404 Not Found");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
        let err = anyhow::anyhow!("Repository not found");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
    }

    /// GitHub sometimes returns 410 Gone for tag refs that were recently
    /// deleted (the ref was tombstoned but still surfaces in the error
    /// shape). Bucket as `AlreadyAbsent` so re-running `--rollback-only`
    /// does not surface a spurious manual-cleanup warn.
    #[test]
    fn classify_delete_error_treats_410_gone_as_already_absent() {
        let err = anyhow::anyhow!("HTTP 410 Gone");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
        // Case-insensitive match — GitHub mixes casing in error payloads.
        let err = anyhow::anyhow!("Resource has been gone");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
    }

    /// `DELETE /git/refs/tags/<tag>` returns 422 Unprocessable Entity on
    /// some "reference does not exist" edge cases (e.g., tag ref was
    /// never created because the release was a draft). Bucket as
    /// `AlreadyAbsent` for the same reason as 410.
    #[test]
    fn classify_delete_error_treats_422_unprocessable_as_already_absent() {
        let err = anyhow::anyhow!("HTTP 422 Unprocessable Entity");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
        let err = anyhow::anyhow!("422: Reference does not exist");
        assert_eq!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::AlreadyAbsent
        );
    }

    /// 5xx is the canonical Failed bucket — genuine transport / auth
    /// errors must still surface the manual-cleanup warn so operators
    /// know to revisit.
    #[test]
    fn classify_delete_error_treats_500_as_failed() {
        let err = anyhow::anyhow!("HTTP 500 internal error");
        assert!(matches!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::Failed(_)
        ));
        let err = anyhow::anyhow!("HTTP 503 Service Unavailable");
        assert!(matches!(
            classify_delete_err(&err),
            ReleaseDeleteOutcome::Failed(_)
        ));
    }

    /// Publisher rollback deletes the RELEASE only. The tag ref has a
    /// single owner — `anodizer tag rollback` — so this path must never
    /// issue a tag delete (a second delete here would race the tag
    /// rollback and run operator hooks between two half-deleted states).
    #[test]
    fn github_release_rollback_deletes_release_only_never_the_tag() {
        let mock = MockGitHubClient::new();
        mock.set_delete_release_response(Ok(()));
        let mock = Arc::new(mock);
        let p = GithubReleasePublisher::with_client(mock.clone());

        let target = GithubReleaseTarget {
            crate_name: "demo".into(),
            owner: "acme".into(),
            repo: "widget".into(),
            tag: "v1.0.0".into(),
            release_id: Some(42),
        };
        let mut evidence = PublishEvidence::new("github-release");
        evidence.extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: vec![target.clone()],
            },
        );

        let mut ctx = TestContextBuilder::new().build();
        p.rollback(&mut ctx, &evidence).expect("rollback ok");

        // The release DELETE fired exactly once with the recorded
        // evidence target's params; no tag DELETE was issued.
        assert_eq!(mock.delete_release_call_count(), 1);
        assert_eq!(mock.delete_tag_call_count(), 0);
        let rel = mock.delete_release_calls();
        assert_eq!(rel[0].owner, "acme");
        assert_eq!(rel[0].repo, "widget");
        assert_eq!(rel[0].release_id, 42);
    }

    #[test]
    fn collect_release_targets_picks_up_per_crate_github_blocks() {
        let ctx = TestContextBuilder::new()
            .crates(vec![github_release_crate("demo")])
            .build();
        let targets = collect_release_targets(&ctx).expect("collect ok");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].crate_name, "demo");
        assert_eq!(targets[0].owner, "acme");
        assert_eq!(targets[0].repo, "widget");
        assert!(targets[0].release_id.is_none(), "id not yet captured");
    }

    /// `is_github_release_configured` (the registry predicate) lives in
    /// `stage-publish/src/registry.rs`. This sibling test pins the
    /// configured/unconfigured boundary inside `stage-release` so a
    /// publisher renaming or moving the `release.github` block surfaces
    /// here too. It exercises `collect_release_targets` because the
    /// registry predicate consults the same fields.
    #[test]
    fn collect_release_targets_skips_when_no_release_block() {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            release: None,
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let targets = collect_release_targets(&ctx).expect("collect ok");
        assert!(targets.is_empty());
    }

    /// Monorepo shape: three crates all configured to publish into one
    /// workspace-wide GitHub release (same owner/repo/tag). The
    /// ID-capture loop MUST collapse the three lookups into one
    /// `get_release_by_tag` round-trip and reuse the cached id for the
    /// remaining two targets.
    #[test]
    fn get_release_by_tag_dedups_repeated_target_tuples() {
        use anodizer_core::github_client::ReleaseInfo;

        let mock = MockGitHubClient::new();
        mock.set_get_release_by_tag_response(Ok(Some(ReleaseInfo {
            id: 99,
            html_url: "https://github.com/acme/widget/releases/tag/v1.0.0".into(),
            tag_name: "v1.0.0".into(),
            name: Some("v1.0.0".into()),
            draft: false,
        })));
        let mock = Arc::new(mock);

        // Three targets sharing one (owner, repo, tag) tuple — the
        // canonical monorepo shape where one workspace-wide release
        // surfaces under multiple crate logical labels.
        let mut targets = vec![
            GithubReleaseTarget {
                crate_name: "demo-core".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "v1.0.0".into(),
                release_id: None,
            },
            GithubReleaseTarget {
                crate_name: "demo-cli".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "v1.0.0".into(),
                release_id: None,
            },
            GithubReleaseTarget {
                crate_name: "demo-helper".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "v1.0.0".into(),
                release_id: None,
            },
        ];

        capture_release_ids(mock.as_ref(), &mut targets);

        // The memo collapsed three logical lookups into one network
        // round-trip; every target inherited the cached release id.
        assert_eq!(
            mock.get_release_by_tag_call_count(),
            1,
            "expected memo to collapse 3 lookups to 1 round-trip"
        );
        assert_eq!(targets[0].release_id, Some(99));
        assert_eq!(targets[1].release_id, Some(99));
        assert_eq!(targets[2].release_id, Some(99));
    }

    /// Negative — when each target points at a distinct `(owner, repo, tag)`
    /// tuple the memo never hits, so N targets produce N round-trips.
    /// Pins that the dedup is keyed on the tuple, not blindly shared.
    #[test]
    fn get_release_by_tag_queries_each_distinct_target_tuple() {
        use anodizer_core::github_client::ReleaseInfo;

        let mock = MockGitHubClient::new();
        mock.set_get_release_by_tag_response(Ok(Some(ReleaseInfo {
            id: 7,
            html_url: "https://github.com/acme/widget/releases/tag/v1.0.0".into(),
            tag_name: "v1.0.0".into(),
            name: None,
            draft: false,
        })));
        let mock = Arc::new(mock);

        let mut targets = vec![
            GithubReleaseTarget {
                crate_name: "alpha".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "alpha/v1.0.0".into(),
                release_id: None,
            },
            GithubReleaseTarget {
                crate_name: "beta".into(),
                owner: "acme".into(),
                repo: "widget".into(),
                tag: "beta/v1.0.0".into(),
                release_id: None,
            },
        ];

        capture_release_ids(mock.as_ref(), &mut targets);
        assert_eq!(mock.get_release_by_tag_call_count(), 2);
    }
}
