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
//! A 404 response buckets as `ReleaseDeleteOutcome::AlreadyAbsent`
//! so re-running `anodizer tag rollback` after a partial success does
//! not surface false failures.
//!
//! # ID capture
//!
//! [`crate::ReleaseStage`]'s body is unchanged per the
//! release-resilience contract. To learn each release's numeric ID
//! (required for `DELETE /releases/{id}`) the publisher queries
//! [`anodizer_core::github_client::GitHubClient::get_release_by_tag`]
//! once per configured (owner, repo, tag) target after `ReleaseStage::run`
//! returns. The same client is reused for both delete operations during
//! rollback. Tests inject a `MockGitHubClient` (gated behind anodizer-core's
//! `test-helpers` feature, so it is absent from the default doc build)
//! via [`GithubReleasePublisher::with_client`].
//!
//! # Credential handling
//!
//! `GithubReleaseTarget` stores `(owner, repo, tag, release_id)` only.
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

    /// Construct with a caller-provided client — the injection point tests use to inject
    /// anodizer-core's `test-helpers`-gated `MockGitHubClient`.
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
pub(crate) fn active_release_configs(ctx: &Context) -> Vec<&anodizer_core::config::CrateConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .selected_crates(selected)
        .into_iter()
        // A crate the release stage skipped has no release to name, capture or
        // roll back; emitting a target for it makes `PublishEvidence.primary_ref`
        // point at a release that was never created.
        .filter(|c| !ctx.stage_outputs.release_skipped_crates.contains(&c.name))
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
    use crate::resolve_release_repo;
    use anodizer_core::release_tag::resolve_release_tag;

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
        let tag = resolve_release_tag(ctx, c, release_cfg.tag.as_deref())?;
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

/// The releases the release stage created or updated this run, as rollback
/// targets. `release_id` is left `None` for [`capture_release_ids`].
fn written_release_targets(ctx: &Context) -> Vec<GithubReleaseTarget> {
    ctx.stage_outputs
        .releases_written
        .iter()
        .map(|w| GithubReleaseTarget {
            crate_name: w.crate_name.clone(),
            owner: w.owner.clone(),
            repo: w.repo.clone(),
            tag: w.tag.clone(),
            release_id: None,
        })
        .collect()
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

/// The evidence for the releases `targets` names.
fn release_evidence(targets: Vec<GithubReleaseTarget>) -> anodizer_core::PublishEvidence {
    let mut evidence = anodizer_core::PublishEvidence::new(GithubReleasePublisher::PUBLISHER_NAME);
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
    evidence
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
        // Release creation + asset upload authenticate with the token the
        // stage resolves for each crate: the `release.<forge>.token` its
        // release block sets, else the run token (`--token`, else the forge's
        // environment variable: the GitHub ladder, GITLAB_TOKEN or
        // GITEA_TOKEN). This requirement is also the release stage's, so it
        // names the forge the stage releases on. Only a selected crate with a
        // `release:` block and no release token of its own needs the run
        // token, so a config that creates no release, or one whose every
        // release names its token, demands nothing. A release token that
        // does not render is reported by the release preflight with its
        // cause, so it does not also ask for the run token here.
        let needs_run_token = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| {
                anodizer_core::config::crate_is_selected(&ctx.options.selected_crates, &c.name)
            })
            .filter_map(|c| c.release.as_ref())
            .any(|release_cfg| matches!(crate::resolve_release_token(ctx, release_cfg), Ok(None)));
        if !needs_run_token || ctx.options.token.as_deref().is_some_and(|t| !t.is_empty()) {
            return Vec::new();
        }
        let owned = |vars: &[&str]| vars.iter().map(|v| v.to_string()).collect();
        vec![match ctx.token_type {
            ScmTokenType::GitHub => anodizer_core::EnvRequirement::EnvAnyOf {
                vars: owned(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER),
            },
            ScmTokenType::GitLab => anodizer_core::EnvRequirement::EnvAllOf {
                vars: owned(&["GITLAB_TOKEN"]),
            },
            ScmTokenType::Gitea => anodizer_core::EnvRequirement::EnvAllOf {
                vars: owned(&["GITEA_TOKEN"]),
            },
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        // The publish itself is the release stage's job; this wrapper adds
        // the (owner, repo, tag) enumeration and the numeric-ID lookup that
        // rollback evidence needs.
        //
        // Unless the pipeline already ran that stage this run: a full
        // `anodizer release` runs `release` and then dispatches this
        // publisher, and delegating a second time creates each release —
        // and fires each nightly retention sweep — twice. Only ID capture
        // is owed in that case.
        if ctx.stage_outputs.release_stage_ran {
            ctx.logger("publish")
                .verbose("github-release: release stage already ran this run; capturing ids only");
        } else if let Err(err) = <ReleaseStage as Stage>::run(&ReleaseStage, ctx) {
            // The stage creates a release per crate and then uploads its
            // assets, so a failure can leave releases behind. The ones the
            // stage wrote before failing are what a rollback has to delete;
            // a release that existed before the run is not its work.
            if !ctx.is_dry_run() && !ctx.is_snapshot() {
                let mut targets = written_release_targets(ctx);
                capture_release_ids(self.client.as_ref(), &mut targets);
                targets.retain(|t| t.release_id.is_some());
                if !targets.is_empty() {
                    ctx.record_pending_evidence(release_evidence(targets));
                }
            }
            return Err(err);
        }

        let mut targets = collect_release_targets(ctx)?;
        // Skip ID capture in dry-run / snapshot — no release was created
        // so `get_release_by_tag` would 404 (or worse, retry-loop on
        // transport errors). Evidence still captures the (owner, repo,
        // tag) tuples so the rollback report names what was targeted.
        if !ctx.is_dry_run() && !ctx.is_snapshot() {
            capture_release_ids(self.client.as_ref(), &mut targets);
        }
        Ok(release_evidence(targets))
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
                ctx,
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

        let retry_scope = anodizer_core::retry::current_scope();
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
                    let retry_scope = retry_scope.clone();
                    handles.push(s.spawn(move || {
                        let _scope = anodizer_core::retry::RetryScope::inherit(retry_scope);
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
            "{} rollback deleted {}, {} already-absent, {} failed \
             (tag refs untouched — tag rollback owns them)",
            Self::PUBLISHER_NAME,
            anodizer_core::text::count_noun(release_deleted, "release", "releases"),
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
    /// configured/selected. See [`crate::preflight::release_preflight`].
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(crate::preflight::release_preflight(ctx))
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
    fn release_preflight_passes_with_no_release_configured() {
        let ctx = TestContextBuilder::new().build();
        let p = GithubReleasePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn release_preflight_blocks_when_no_token_resolves() {
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

    /// A release whose `release.github.token` renders a value authenticates
    /// with it, so the requirement asks for no run token, and the token the
    /// release stage would send is that one. A crate with no release token
    /// of its own still needs the run token.
    #[test]
    fn the_release_requirement_reads_the_release_token_the_stage_sends() {
        let mut tokened = github_release_crate("demo");
        tokened
            .release
            .as_mut()
            .unwrap()
            .github
            .as_mut()
            .unwrap()
            .token = Some("{{ .Env.RELEASE_TOKEN }}".into());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![tokened.clone()])
            .sealed_env()
            .build();
        ctx.template_vars_mut()
            .set_env("RELEASE_TOKEN", "release-tok");
        let p = GithubReleasePublisher::new();
        assert_eq!(
            p.requirements(&ctx),
            Vec::new(),
            "a release token that renders needs no run token"
        );
        assert_eq!(
            crate::resolve_release_token(&ctx, tokened.release.as_ref().unwrap())
                .unwrap()
                .as_deref(),
            Some("release-tok"),
            "the release stage sends the release token the requirement relied on"
        );

        let mut ctx = TestContextBuilder::new()
            .crates(vec![tokened, github_release_crate("other")])
            .sealed_env()
            .build();
        ctx.template_vars_mut()
            .set_env("RELEASE_TOKEN", "release-tok");
        assert_eq!(
            p.requirements(&ctx),
            vec![anodizer_core::EnvRequirement::EnvAnyOf {
                vars: anodizer_core::git::GITHUB_TOKEN_ENV_LADDER
                    .iter()
                    .map(|v| v.to_string())
                    .collect(),
            }],
            "a crate with no release token of its own needs the run token"
        );
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
    /// shape). Bucket as `AlreadyAbsent` so re-running
    /// `anodizer tag rollback` does not surface a spurious manual-cleanup warn.
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

    /// A crate the release stage skipped has no release: emitting a target
    /// for it makes the id lookup 404 and puts a non-existent release in
    /// `PublishEvidence.primary_ref`.
    #[test]
    fn collect_release_targets_excludes_crates_the_release_stage_skipped() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                github_release_crate("demo"),
                github_release_crate("other"),
            ])
            .build();
        ctx.stage_outputs.release_skipped_crates = vec!["demo".to_string()];
        let targets = collect_release_targets(&ctx).expect("collect ok");
        let names: Vec<&str> = targets.iter().map(|t| t.crate_name.as_str()).collect();
        assert_eq!(names, vec!["other"]);
    }

    /// The tag the publisher captures by MUST be the tag the release stage
    /// created on. With `nightly.tag_name` set they are only equal if both
    /// route through the same resolver.
    #[test]
    fn collect_release_targets_uses_the_nightly_tag_name_the_release_was_created_on() {
        let track = |name: &str, tmpl: &str| {
            let mut c = github_release_crate(name);
            c.tag_template = Some(tmpl.to_string()); // tag-family-ok: a fixture WRITES the field; the read-through rule governs readers
            c
        };
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                track("app", "v{{ Version }}"),
                track("operator", "operator-v{{ Version }}"),
            ])
            .build();
        ctx.options.nightly = true;
        ctx.config.nightly = Some(anodizer_core::config::NightlyConfig {
            tag_name: Some("edge".to_string()),
            ..Default::default()
        });
        let targets = collect_release_targets(&ctx).expect("collect ok");
        let tags: Vec<&str> = targets.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(tags, vec!["vedge", "operator-vedge"]);
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

    fn one_target(tag: &str) -> GithubReleaseTarget {
        GithubReleaseTarget {
            crate_name: "demo".into(),
            owner: "acme".into(),
            repo: "widget".into(),
            tag: tag.into(),
            release_id: Some(999), // sentinel — must be overwritten by capture
        }
    }

    #[test]
    fn capture_release_ids_memoizes_identical_tuples_to_one_query() {
        // Two targets sharing (owner, repo, tag) — the common monorepo shape
        // (one workspace tag → one release). The memo must collapse them to a
        // single API round-trip and reuse the cached id for the duplicate.
        let mock = MockGitHubClient::new();
        mock.set_get_release_by_tag_response(Ok(Some(anodizer_core::github_client::ReleaseInfo {
            id: 4242,
            html_url: "https://github.com/acme/widget/releases/4242".into(),
            tag_name: "v1.0.0".into(),
            name: None,
            draft: false,
        })));
        let mock = Arc::new(mock);
        let mut targets = vec![one_target("v1.0.0"), one_target("v1.0.0")];
        capture_release_ids(mock.as_ref(), &mut targets);
        assert_eq!(
            mock.get_release_by_tag_call_count(),
            1,
            "the duplicate tuple must be served from the memo, not re-queried"
        );
        assert_eq!(targets[0].release_id, Some(4242));
        assert_eq!(
            targets[1].release_id,
            Some(4242),
            "the memoized id must backfill the duplicate target"
        );
    }

    #[test]
    fn capture_release_ids_sets_none_when_tag_has_no_release() {
        // Ok(None) — the tag exists but has no release; the id must clear to
        // None so rollback skips the release-delete for this row.
        let mock = MockGitHubClient::new();
        mock.set_get_release_by_tag_response(Ok(None));
        let mock = Arc::new(mock);
        let mut targets = vec![one_target("v1.0.0")];
        capture_release_ids(mock.as_ref(), &mut targets);
        assert_eq!(
            targets[0].release_id, None,
            "no release for the tag must clear the sentinel id to None"
        );
    }

    #[test]
    fn capture_release_ids_swallows_lookup_errors_to_none() {
        // A transport/auth error during the post-publish enrichment must NOT
        // fail the run; the id clears to None (rollback skips the delete) and
        // the publish's already published evidence is preserved.
        let mock = MockGitHubClient::new();
        mock.set_get_release_by_tag_response(Err("HTTP 503 Service Unavailable".to_string()));
        let mock = Arc::new(mock);
        let mut targets = vec![one_target("v1.0.0")];
        capture_release_ids(mock.as_ref(), &mut targets);
        assert_eq!(
            targets[0].release_id, None,
            "a lookup error must clear the id to None, not abort"
        );
    }

    #[test]
    fn github_release_rollback_skips_delete_when_no_release_id() {
        // A target with no captured release_id: rollback must NOT issue a
        // release DELETE (it would 404), treat the row as already-absent, and
        // still return Ok.
        let mock = MockGitHubClient::new();
        mock.set_delete_release_response(Ok(()));
        let mock = Arc::new(mock);
        let p = GithubReleasePublisher::with_client(mock.clone());

        let target = GithubReleaseTarget {
            crate_name: "demo".into(),
            owner: "acme".into(),
            repo: "widget".into(),
            tag: "v1.0.0".into(),
            release_id: None,
        };
        let mut evidence = PublishEvidence::new("github-release");
        evidence.extra = anodizer_core::PublishEvidenceExtra::GithubRelease(
            anodizer_core::publish_evidence::GithubReleaseExtra {
                github_release_targets: vec![target],
            },
        );
        let mut ctx = TestContextBuilder::new().build();
        p.rollback(&mut ctx, &evidence)
            .expect("rollback with no captured id returns Ok");
        assert_eq!(
            mock.delete_release_call_count(),
            0,
            "no release_id => no DELETE is issued for this row"
        );
    }

    /// A crate whose release block trips `validate_release_flags` the moment
    /// the release stage enters its per-crate loop. That makes "did the
    /// publisher delegate to the stage?" observable without a live SCM:
    /// `Err` means it delegated, `Ok` means it did not.
    fn tripwire_release_crate() -> CrateConfig {
        CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            release: Some(ReleaseConfig {
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
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

    /// A full `anodizer release` runs the release stage AND dispatches this
    /// publisher, which wraps that same stage. Delegating a second time
    /// creates every release twice and fires the nightly retention sweep
    /// twice — the sweep's second pass seeing its own first-pass release.
    #[test]
    fn publisher_run_skips_delegation_when_the_release_stage_already_ran() {
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![tripwire_release_crate()])
            .build();
        ctx.options.dry_run = true;
        ctx.stage_outputs.release_stage_ran = true;
        let p = GithubReleasePublisher::with_client(Arc::new(MockGitHubClient::new()));
        let evidence = p
            .run(&mut ctx)
            .expect("the already-ran marker must stop a second delegation");
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://github.com/acme/widget/releases/tag/v1.0.0"),
            "id capture still runs — only the delegation is skipped"
        );
    }

    /// Multitrack shape — the one the family-scoped retention sweep and the
    /// double-create both bite hardest. Three tracks, one publisher dispatch:
    /// every track's id is captured and NONE of them is released a second
    /// time.
    #[test]
    fn publisher_run_captures_every_track_without_re_releasing_any() {
        let track = |name: &str, tmpl: &str| {
            let mut c = tripwire_release_crate();
            c.name = name.to_string();
            c.tag_template = Some(tmpl.to_string()); // tag-family-ok: a fixture WRITES the field; the read-through rule governs readers
            c
        };
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![
                track("app", "v{{ Version }}"),
                track("operator", "operator-v{{ Version }}"),
                track("csi", "csi-v{{ Version }}"),
            ])
            .build();
        ctx.options.dry_run = true;
        ctx.stage_outputs.release_stage_ran = true;
        let p = GithubReleasePublisher::with_client(Arc::new(MockGitHubClient::new()));
        let evidence = p.run(&mut ctx).expect("no second delegation for any track");
        let targets = decode_github_release_targets(&evidence.extra);
        assert_eq!(
            targets.iter().map(|t| t.tag.as_str()).collect::<Vec<_>>(),
            vec!["v1.0.0", "operator-v1.0.0", "csi-v1.0.0"],
        );
    }

    /// The other half: with no marker (the `--publish-only` shape, where the
    /// release stage never ran) the publisher MUST still delegate.
    #[test]
    fn publisher_run_delegates_when_the_release_stage_has_not_run() {
        let mut ctx = TestContextBuilder::new()
            .tag("v1.0.0")
            .crates(vec![tripwire_release_crate()])
            .build();
        ctx.options.dry_run = true;
        let p = GithubReleasePublisher::with_client(Arc::new(MockGitHubClient::new()));
        let err = p
            .run(&mut ctx)
            .expect_err("without the marker the publisher owns the release");
        assert!(
            err.to_string().contains("replace_existing_draft"),
            "the delegated stage must have entered its per-crate loop; got: {err}"
        );
    }

    /// A delegated stage that fails can have created releases already. The
    /// ones the stage wrote this run are kept for the `Failed` row; a
    /// release that existed before the run, which the stage never touched,
    /// is not this run's work and is not recorded.
    #[test]
    fn a_failed_delegation_keeps_only_the_releases_the_stage_wrote() {
        let run = |written: bool, existing: Option<u64>| {
            let mut ctx = TestContextBuilder::new()
                .tag("v1.0.0")
                .crates(vec![tripwire_release_crate()])
                .build();
            if written {
                ctx.stage_outputs
                    .releases_written
                    .push(anodizer_core::context::WrittenRelease {
                        crate_name: "demo".into(),
                        owner: "acme".into(),
                        repo: "widget".into(),
                        tag: "v1.0.0".into(),
                        url: "https://github.com/acme/widget/releases/tag/v1.0.0".into(),
                        draft: false,
                        assets: 0,
                    });
            }
            let mock = MockGitHubClient::new();
            mock.set_get_release_by_tag_response(Ok(existing.map(|id| {
                anodizer_core::github_client::ReleaseInfo {
                    id,
                    html_url: "https://github.com/acme/widget/releases/1".into(),
                    tag_name: "v1.0.0".into(),
                    name: None,
                    draft: false,
                }
            })));
            GithubReleasePublisher::with_client(Arc::new(mock))
                .run(&mut ctx)
                .expect_err("the tripwire crate fails the delegated stage");
            ctx.take_pending_evidence()
        };

        let kept = run(true, Some(7)).expect("the release the stage wrote is recorded");
        let targets = decode_github_release_targets(&kept.extra);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].release_id, Some(7));
        assert!(kept.records_published_work());

        assert!(
            run(false, Some(7)).is_none(),
            "a release that existed before the run is not its work"
        );
        assert!(run(true, None).is_none());
    }
}
