//! Post-publish moderation polling: the shared chocolatey/winget eligibility
//! ladder and the parallel poller runner.

use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig, PublishConfig, WingetConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::{chocolatey, crates_with_publisher, post_publish, winget};

/// Read access to a publisher config's optional `post_publish_poll` block,
/// so [`poll_eligibility`] can gate moderation polling generically across
/// the chocolatey and winget arms (whose ladders are otherwise identical).
pub(crate) trait HasPostPublishPoll {
    fn post_publish_poll(&self) -> Option<PostPublishPollConfig>;
}

impl HasPostPublishPoll for ChocolateyConfig {
    fn post_publish_poll(&self) -> Option<PostPublishPollConfig> {
        self.post_publish_poll
    }
}

impl HasPostPublishPoll for WingetConfig {
    fn post_publish_poll(&self) -> Option<PostPublishPollConfig> {
        self.post_publish_poll
    }
}

/// One crate's resolved post-publish poll eligibility, yielded by
/// [`poll_eligibility`].
///
/// `poll_cfg` is `Some` when a poll job should run, and `None` when the
/// crate is eligible but polling was skipped via `--no-post-publish-poll`
/// (the caller still records a `NotPolled` summary entry for it).
pub(crate) struct PollCandidate<C> {
    pub(crate) crate_name: String,
    cfg: C,
    pub(crate) poll_cfg: Option<PostPublishPollConfig>,
}

/// Walk the shared chocolatey/winget poll-eligibility ladder and return one
/// [`PollCandidate`] per crate that should be considered for moderation
/// polling.
///
/// Identical semantics for both publishers (divergence here gates
/// irreversible-publisher moderation polling, so it would be a correctness
/// gap): the publisher must not be deselected (`--skip` / `--publishers`),
/// the crate must carry the publisher's config block, and that block's
/// `post_publish_poll.enabled` must not be `false`. `--no-post-publish-poll`
/// (`skip_via_cli`) yields candidates with `poll_cfg: None`; otherwise
/// [`post_publish::resolve_poll_config`] supplies the effective config.
///
/// `selector` picks the per-publisher config off a [`PublishConfig`]
/// (`|p| p.chocolatey.clone()` / `|p| p.winget.clone()`); each arm builds
/// its own `PollJob` payload (package name, token) from the yielded tuples.
pub(crate) fn poll_eligibility<C, F>(
    ctx: &Context,
    selected: &[String],
    publisher: &str,
    skip_via_cli: bool,
    selector: F,
) -> Vec<PollCandidate<C>>
where
    C: HasPostPublishPoll,
    F: Fn(&PublishConfig) -> Option<C>,
{
    let mut out = Vec::new();
    if ctx.publisher_deselected(publisher) {
        return out;
    }
    for crate_name in crates_with_publisher(ctx, selected, |p| selector(p).is_some()) {
        let cfg_opt = ctx
            .config
            .find_crate(&crate_name)
            .and_then(|c| c.publish.as_ref())
            .and_then(&selector);
        let Some(cfg) = cfg_opt else {
            continue;
        };
        // Per-publisher `enabled: false` opts out entirely — distinct from
        // the global `--no-post-publish-poll` skip — so emit no candidate at
        // all (otherwise the renderer would misreport the disabled publisher
        // as "skipped via flag").
        if !cfg.post_publish_poll().unwrap_or_default().enabled {
            continue;
        }
        if skip_via_cli {
            out.push(PollCandidate {
                crate_name,
                cfg,
                poll_cfg: None,
            });
            continue;
        }
        // `resolve_poll_config` collapses the CLI + per-pub gates into one
        // `Option`; the enabled case is already filtered above, so a `None`
        // here can only be the CLI flag — handled by the branch above.
        let Some(poll_cfg) = post_publish::resolve_poll_config(ctx, cfg.post_publish_poll()) else {
            continue;
        };
        out.push(PollCandidate {
            crate_name,
            cfg,
            poll_cfg: Some(poll_cfg),
        });
    }
    out
}

/// Build the post-publish polling job list from the active context and run
/// every job in parallel. Writes typed `PostPublishResult` entries (as JSON
/// values) into `ctx.stage_outputs.post_publish_results` for the deferred
/// release-summary renderer to consume.
///
/// Eligibility rules:
///
/// - The publish stage must NOT be in dry-run / snapshot mode (gated at
///   the call site — nothing was actually pushed in those modes).
/// - Chocolatey jobs require `--skip=choco` to be absent AND a per-crate
///   `chocolatey:` block with `post_publish_poll.enabled != false`.
/// - WinGet jobs require `--skip=winget` to be absent AND a per-crate
///   `winget:` block with `post_publish_poll.enabled != false`.
/// - `--no-post-publish-poll` short-circuits to a `NotPolled` result per
///   eligible publisher (so the release summary can render "skipped"
///   distinctly from "no publishers configured").
///
/// All polling is non-fatal; any worker error becomes a
/// `PostPublishStatus::Error` in the results vec rather than failing the
/// publish stage.
///
/// Results are written in dispatch order — crate order within a publisher,
/// chocolatey before winget — whatever each candidate resolved to: a polled
/// result, a `NotPolled` row, or the `Error` row of a candidate that could not
/// be turned into a job at all.
pub(crate) fn run_post_publish_pollers(ctx: &mut Context, selected: &[String], log: &StageLogger) {
    let version = ctx.version();
    let mut jobs: Vec<post_publish::PollJob> = Vec::new();
    // Mirrors `jobs` for the skip-path: when the CLI flag is set no
    // `PollJob` is constructed (no cfg / no URL / no token needed),
    // but a `NotPolled` result is still emitted per configured
    // publisher so summaries can render "skipped via flag" vs. "no
    // publishers configured" distinctly.
    let mut skipped: Vec<(usize, &'static str, String, String)> = Vec::new();
    // Candidates that could not be resolved into a poll job at all. Recorded
    // as `Error` rows so a resolution failure is visible in the release
    // summary instead of silently narrowing the polled set.
    let mut errors: Vec<(usize, post_publish::PostPublishResult)> = Vec::new();
    // The three row classes are filled by one walk but finished at different
    // times, so each row records the position it was issued at and the classes
    // are merged back on it — an unresolvable candidate keeps its place in the
    // listing instead of sinking to the end.
    let mut ord = 0usize;
    let mut job_ord: Vec<usize> = Vec::new();
    let skip_via_cli = ctx.options.skip_post_publish_poll;

    // Chocolatey eligibility — `poll_eligibility` owns the shared ladder
    // (deselected → per-crate `chocolatey:` block → `enabled` → skip_via_cli
    // → `resolve_poll_config`). `publisher_deselected("chocolatey")` folds in
    // both `--skip=chocolatey` and a `--publishers` allowlist that excludes
    // it — a publisher the dispatch loop never ran must never be polled.
    for cand in poll_eligibility(ctx, selected, "chocolatey", skip_via_cli, |p| {
        p.chocolatey.clone()
    }) {
        let PollCandidate {
            crate_name,
            cfg,
            poll_cfg,
        } = cand;
        // Moderation polling scrapes the community gallery's version page —
        // a private/self-hosted feed has no moderation queue and no such
        // page, so a non-community push target is never polled.
        if !chocolatey::targets_community_gallery(&cfg) {
            log.status(&format!(
                "skipped chocolatey moderation polling for '{}' — its push target '{}' is \
                 not the community gallery (moderation applies to the community gallery only)",
                cfg.name.as_deref().unwrap_or(&crate_name),
                chocolatey::push_source(&cfg)
            ));
            continue;
        }
        let pkg_name = cfg.name.unwrap_or(crate_name);
        match poll_cfg {
            None => skipped.push((ord, "chocolatey", pkg_name, version.clone())),
            Some(poll_cfg) => {
                job_ord.push(ord);
                jobs.push(post_publish::PollJob::Chocolatey {
                    package: pkg_name,
                    version: version.clone(),
                    page_base_url: "https://community.chocolatey.org".to_string(),
                    cfg: poll_cfg,
                });
            }
        }
        ord += 1;
    }

    // WinGet eligibility — same shared ladder via `poll_eligibility`. The PR
    // is rediscovered via the GitHub search API (mirroring `preflight::Winget`),
    // so no PR URL needs threading from the publish step.
    for cand in poll_eligibility(ctx, selected, "winget", skip_via_cli, |p| p.winget.clone()) {
        let PollCandidate {
            crate_name,
            cfg,
            poll_cfg,
        } = cand;
        // PackageIdentifier resolution: prefer explicit `package_identifier`,
        // fall back to `<publisher>.<name>` (the upstream convention enforced
        // by winget validation), then to the crate name as a last resort.
        // Under `--strict` the render is an error, and an identifier that did
        // not render must never reach the poll listing (or the GitHub search
        // it drives) as a literal `{{ … }}`: the candidate becomes an `Error`
        // row instead, keyed on the crate name, which is never a template.
        let cfg = match winget::derive_winget_config(ctx, log, &cfg, &crate_name) {
            Ok(derived) => derived,
            Err(e) => {
                errors.push((
                    ord,
                    post_publish::PostPublishResult {
                        publisher: "winget".to_string(),
                        package: crate_name.clone(),
                        version: version.clone(),
                        status: post_publish::PostPublishStatus::Error {
                            reason: e.to_string(),
                        },
                    },
                ));
                ord += 1;
                continue;
            }
        };
        let pkg_id = winget::package_identifier_of(&cfg);
        match poll_cfg {
            None => skipped.push((ord, "winget", pkg_id, version.clone())),
            Some(poll_cfg) => {
                // Render a configured `repository.token` before use — a
                // templated `{{ .Env.GH_PAT }}` must become the resolved
                // credential, not the literal template string, for the poll's
                // GitHub API auth.
                let explicit = cfg
                    .repository
                    .as_ref()
                    .and_then(|r| r.token.as_deref())
                    .and_then(|t| match ctx.render_template(t) {
                        Ok(rendered) => Some(rendered),
                        // On render failure, drop to the env fallback rather than
                        // feeding the raw `{{...}}` template as a literal bearer
                        // token (which would yield an opaque poll auth-failure).
                        Err(e) => {
                            ctx.logger("publish").warn(&format!(
                                "winget post-publish poll: could not render repository.token template ({e}); using environment token"
                            ));
                            None
                        }
                    });
                // Canonical resolver: empty-filters the rendered token AND the
                // env fallbacks (a missing-secret `""` must not be the token).
                let token = anodizer_core::git::resolve_github_token_with_env(
                    explicit.as_deref(),
                    &|key| ctx.env_var(key),
                );
                // Poll the same upstream the publisher submitted its PR to,
                // and drop the in:title precision when a custom template
                // makes the PR title unpredictable — mirroring the burn
                // probe so poll and publish can never disagree.
                let (upstream_owner, upstream_repo) = winget::resolve_winget_upstream(&cfg);
                job_ord.push(ord);
                jobs.push(post_publish::PollJob::Winget {
                    package_identifier: pkg_id,
                    version: version.clone(),
                    api_base_url: "https://api.github.com".to_string(),
                    upstream_slug: format!("{upstream_owner}/{upstream_repo}"),
                    search_in_title: cfg.commit_msg_template.is_none(),
                    token,
                    cfg: poll_cfg,
                });
            }
        }
        ord += 1;
    }

    // Skip-path: emit one `NotPolled` per eligible publisher so the
    // release summary distinguishes "skipped via --no-post-publish-poll"
    // from "no eligible publishers". Short-circuits without running any
    // pollers.
    if skip_via_cli {
        if skipped.is_empty() && errors.is_empty() {
            log.verbose(
                "skipped post-publish polling — --no-post-publish-poll (no eligible publishers)",
            );
            return;
        }
        log.verbose(&format!(
            "skipped post-publish polling — --no-post-publish-poll ({} publisher(s) recorded as NotPolled)",
            skipped.len()
        ));
        let mut rows: Vec<(usize, post_publish::PostPublishResult)> = skipped
            .into_iter()
            .map(|(ord, publisher, package, version)| {
                (
                    ord,
                    post_publish::PostPublishResult {
                        publisher: publisher.to_string(),
                        package,
                        version,
                        status: post_publish::PostPublishStatus::NotPolled,
                    },
                )
            })
            .collect();
        rows.extend(errors);
        rows.sort_by_key(|(ord, _)| *ord);
        ctx.stage_outputs.post_publish_results = rows
            .iter()
            .map(|(_, r)| {
                serde_json::to_value(r).expect(
                    "PostPublishResult is always serializable — schema is derived from a string + enum struct",
                )
            })
            .collect();
        return;
    }

    if jobs.is_empty() && errors.is_empty() {
        log.verbose("no eligible publishers for post-publish polling");
        return;
    }
    if !jobs.is_empty() {
        log.status(&format!(
            "starting {} parallel post-publish poller(s)",
            jobs.len()
        ));
    }
    // The runner returns its results in input order, so zipping the job
    // ordinals back on is sound.
    let mut rows: Vec<(usize, post_publish::PostPublishResult)> = job_ord
        .into_iter()
        .zip(post_publish::run_post_publish_polls(jobs, log))
        .collect();
    rows.extend(errors);
    rows.sort_by_key(|(ord, _)| *ord);
    let results: Vec<post_publish::PostPublishResult> = rows.into_iter().map(|(_, r)| r).collect();
    for r in &results {
        match &r.status {
            post_publish::PostPublishStatus::Approved { detail } => log.status(&format!(
                "post-publish {} {} {} approved — {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Rejected { detail } => log.warn(&format!(
                "post-publish {} {} {} rejected — {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Timeout { last_state, .. } => log.warn(&format!(
                "post-publish {} {} {} polling timed out (last state: {})",
                r.publisher, r.package, r.version, last_state
            )),
            post_publish::PostPublishStatus::Error { reason } => log.warn(&format!(
                "post-publish {} {} {} polling error: {}",
                r.publisher, r.package, r.version, reason
            )),
            post_publish::PostPublishStatus::Pending { .. }
            | post_publish::PostPublishStatus::NotPolled => {
                // Pending shouldn't reach this path (poller loops until
                // terminal). NotPolled is built by callers that explicitly
                // opt out — silent is fine.
            }
        }
    }
    ctx.stage_outputs.post_publish_results = results
        .into_iter()
        .map(|r| {
            serde_json::to_value(&r).expect(
                "PostPublishResult is always serializable — schema is derived from a string + enum struct",
            )
        })
        .collect();
}
