//! `HomebrewPublisher` — Manager-group `Publisher` impl that wraps the
//! existing [`publish_to_homebrew`](super::publish_to_homebrew)
//! (per-crate formula + optional same-tap cask) and
//! [`publish_top_level_homebrew_casks`](super::publish_top_level_homebrew_casks)
//! (top-level `homebrew_casks:` block).
//!
//! Rollback shape: every push to a publisher-owned tap is recorded in
//! `PublishEvidence.extra` as a target with the cloned repo URL +
//! branch. At rollback time the helper re-clones, runs
//! `git revert HEAD --no-edit`, and pushes back to the same branch.
//!
//! The publish path itself (in [`super::publish_formula`] /
//! [`super::publish_top`]) is unchanged: those entry-points still
//! clone into a `tempfile::tempdir()` and drop the clone at the end
//! of the call. This publisher captures the re-clone parameters from
//! the live config *before* `publish_to_homebrew` runs, then records
//! them after a successful push so a later `--rollback-only` has
//! everything it needs without depending on the ephemeral tempdir.
//!
//! CREDENTIAL HANDLING: [`HomebrewTarget`] stores `token_env_var` —
//! the NAME of the env var to consult at rollback time — not the
//! resolved token VALUE. The actual token is read from the live env
//! at yank time so persisted evidence (`dist/run-<id>/report.json`,
//! the announce-time release-body summary) carries no secret
//! material. Same rule applies to the scoop / nix git-revert
//! publishers and is documented at their module level.

use anodizer_core::context::Context;
use anodizer_core::publish_evidence::{
    HomebrewExtra, HomebrewTargetSnapshot, PublishEvidenceExtra,
};

simple_publisher!(
    HomebrewPublisher,
    "homebrew",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

/// Serialized shape of a recorded homebrew tap push. Aliased to the
/// core-owned snapshot so the wire schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. One entry per pushed formula/cask.
type HomebrewTarget = HomebrewTargetSnapshot;

/// Decode the `homebrew_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
///
/// Returns an empty Vec when the variant is not `Homebrew` or when its
/// list is empty. The rollback path treats empty-decode the same as
/// no-evidence and emits the canonical empty-evidence warn.
fn decode_homebrew_targets(extra: &PublishEvidenceExtra) -> Vec<HomebrewTarget> {
    match extra {
        PublishEvidenceExtra::Homebrew(h) => h.homebrew_targets.clone(),
        _ => Vec::new(),
    }
}

/// Collapse the recorded tap-push targets to a unique set keyed by
/// `(repo_url, branch)`. The first entry seen wins (so its `target`
/// label surfaces in warn lines).
///
/// One tap can hold many formulae/casks across different crates: if
/// the rollback issued `git revert HEAD --no-edit` twice against the
/// same tap, the second revert would undo the first, silently
/// restoring the bad release. Dedup before fan-out so each tap is
/// reverted exactly once. See module rustdoc.
fn dedup_homebrew_targets(targets: &[HomebrewTarget]) -> Vec<HomebrewTarget> {
    let mut seen: std::collections::BTreeSet<(String, Option<String>)> =
        std::collections::BTreeSet::new();
    let mut out: Vec<HomebrewTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        let key = (t.repo_url.clone(), t.branch.clone());
        if seen.insert(key) {
            out.push(t.clone());
        }
    }
    out
}

/// Build the list of (target, RepositoryConfig, token) triples for
/// every homebrew push this run would record. Reads `ctx.config`
/// only — does not touch the artifact tree — so it stays safe to
/// call before `run` fires and after `rollback` is requested.
fn collect_run_targets(ctx: &Context) -> Vec<HomebrewTarget> {
    let mut out: Vec<HomebrewTarget> = Vec::new();

    // Per-crate formulae (and same-tap casks share the formula's tap).
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(hb) = c.publish.as_ref().and_then(|p| p.homebrew.as_ref()) else {
            continue;
        };
        if let Some((owner, name)) = crate::util::resolve_repo_owner_name(hb.repository.as_ref()) {
            out.push(HomebrewTarget {
                target: c.name.clone(),
                repo_url: format!("https://github.com/{}/{}.git", owner, name),
                branch: crate::util::resolve_branch(ctx, hb.repository.as_ref()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".to_string()),
            });
        }
    }

    // Top-level homebrew_casks. The dispatch in `publish_top.rs` walks
    // every entry; mirror that walk so every published cask gets a
    // rollback record.
    if let Some(casks) = ctx.config.homebrew_casks.as_ref() {
        for cask in casks {
            let label = cask.name.clone().unwrap_or_else(|| "homebrew_casks".into());
            if let Some((owner, name)) =
                crate::util::resolve_repo_owner_name(cask.repository.as_ref())
            {
                out.push(HomebrewTarget {
                    target: label,
                    repo_url: format!("https://github.com/{}/{}.git", owner, name),
                    branch: crate::util::resolve_branch(ctx, cask.repository.as_ref()),
                    token_env_var: Some("HOMEBREW_TAP_TOKEN".to_string()),
                });
            }
        }
    }

    out
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "starting homebrew publish — scanning {} selected crate(s) for a homebrew config block",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.homebrew` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!(
        "skipped homebrew for crate '{}' — no homebrew config block",
        crate_name
    )
}

/// Message emitted just before delegating to `publish_to_homebrew`.
/// Anchors the homebrew activity (formula render, tap clone, push) to a
/// specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate homebrew publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit.
///
/// `processed` is the count of crates the publisher invoked
/// `publish_to_homebrew` on for the per-crate FORMULA surface (not the
/// count of successful tap pushes — `publish_to_homebrew` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line). `casks` is the number of in-scope top-level `homebrew_casks:`
/// entries the run rendered.
///
/// Both surfaces count toward the unit total: a cask-only project (the
/// recommended path, zero formula blocks) would otherwise report
/// `0 unit(s) processed` even after pushing its cask, which reads as a
/// no-op to operators scanning the log.
pub(crate) fn run_done_message(processed: usize, casks: usize) -> String {
    format!(
        "finished homebrew publish — {} configured unit(s) processed ({} formula crate(s), {} cask(s))",
        processed + casks,
        processed,
        casks,
    )
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_homebrew_per_crate_configured`
/// check passed (i.e. crates the publisher actually iterated). `selected_len`
/// is the size of the implicit-all-resolved selection.
///
/// The dry-run / skip_upload paths inside `publish_to_homebrew` return
/// Ok(false) without pushing — `processed` must still increment for them,
/// otherwise this predicate fires a false-positive warning even though the
/// correct code path ran. Incrementing only on push-success would
/// short-circuit this predicate to `true` in dry-run with a configured
/// crate.
///
/// `cask_total` is the number of top-level `homebrew_casks:` entries the
/// run dispatched. The homebrew publisher has TWO sub-surfaces — the
/// per-crate `publish.homebrew` FORMULA and the top-level
/// `homebrew_casks:` CASK — and a cask-only project (the recommended path,
/// zero formula blocks) publishes its cask correctly with `processed == 0`.
/// The warning is a true "nothing pushed" signal only when NEITHER surface
/// had anything to publish, so any configured cask suppresses it.
pub(crate) fn should_warn_no_eligible(
    processed: usize,
    selected_len: usize,
    cask_total: usize,
) -> bool {
    processed == 0 && selected_len > 0 && cask_total == 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.homebrew` block at the config level) but NEITHER
/// homebrew sub-surface published anything: the run path processed zero
/// formula crates AND no top-level `homebrew_casks:` entry was configured.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a
/// `publish.homebrew` block — so a zero-processed run means
/// `--crate`/`--all` matrix selection was non-empty AND filtered every
/// homebrew-configured crate out. Operators must see this — otherwise
/// the publisher's `succeeded` status hides the fact that nothing was
/// pushed. Names both surfaces (`publish.homebrew` formula and
/// `homebrew_casks:` cask) since either would satisfy the publisher.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "homebrew publisher registered but 0 of {} effective crate(s) had a homebrew \
         config block and no top-level `homebrew_casks:` were configured — nothing \
         pushed. Check that --crate / --all selects a crate whose `publish.homebrew` \
         block is set, or configure a top-level `homebrew_casks:` entry.",
        selected_total
    )
}

impl anodizer_core::Publisher for HomebrewPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        let mut out = Vec::new();
        let formula_repos = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.homebrew.as_ref())
            .filter(|h| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    h.skip_upload.as_ref(),
                    h.if_condition.as_deref(),
                )
            })
            .map(|h| h.repository.as_ref());
        let cask_repos = ctx
            .config
            .homebrew_casks
            .iter()
            .flatten()
            .filter(|c| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    c.skip_upload.as_ref(),
                    c.if_condition.as_deref(),
                )
            })
            .map(|c| c.repository.as_ref());
        for repo in formula_repos.chain(cask_repos) {
            out.extend(crate::publisher_helpers::git_repo_requirements(
                ctx,
                repo,
                Some("HOMEBREW_TAP_TOKEN"),
            ));
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");

        // Per-crate formulae (delegates to the existing entrypoint).
        // Each call returns `true` when it actually pushed to its tap,
        // `false` when it skipped (skip_upload, dry-run, no config).
        // Aggregate so the evidence only carries rollback targets for
        // taps this run actually mutated — phantom evidence causes the
        // orchestrator to git-revert HEAD in clones that were never
        // touched, which both fails on missing identity AND would
        // otherwise revert the wrong commit (`HEAD` = whatever was on
        // remote before, NOT this run's work).
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_homebrew_per_crate_configured,
        );
        log.status(&run_start_message(selected.len()));
        // `processed` counts crates whose configured predicate passed and
        // whose `publish_to_homebrew` invocation was reached — NOT crates
        // that pushed. The dry-run / skip_upload paths inside
        // `publish_to_homebrew` return Ok(false) without pushing; that's
        // still a successful run of the correct code path, so it must
        // not trigger the no-eligible-crates warning. `any_pushed` (below)
        // tracks the orthogonal "did we mutate a tap" question used to
        // gate evidence recording.
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_homebrew_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &run_skip_unconfigured_message(crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered formula carries the crate's version, not the first
            // crate's (workspace per-crate independent-version mode).
            let pushed = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| super::publish_to_homebrew(ctx, crate_name, &log),
            )?;
            if pushed {
                any_pushed = true;
            }
        }
        // Top-level casks (single invocation; the entrypoint itself
        // iterates over `ctx.config.homebrew_casks`).
        let cask_result = super::publish_top_level_homebrew_casks(ctx, &log)?;
        if cask_result.pushed_any {
            any_pushed = true;
        }

        if should_warn_no_eligible(processed, selected.len(), cask_result.total) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed, cask_result.applicable));
        }

        // Aggregate applicability: when the current crate scope had no
        // per-crate `publish.homebrew` block AND every configured
        // top-level cask was inapplicable (no macOS artifact in scope),
        // record `Skipped(NotApplicable)` so the publisher summary and
        // submitter-gate logic see a non-failure outcome. Conditional on
        // `pending_outcome.is_none()` so sticky-pending signals already
        // recorded by `publish_top_level_homebrew_casks` (PR-already-
        // exists skips, etc.) are not overwritten.
        let nothing_applicable =
            processed == 0 && cask_result.total > 0 && cask_result.applicable == 0;
        if nothing_applicable && ctx.pending_outcome.is_none() {
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
                anodizer_core::SkipReason::NotApplicable,
            ));
        }

        let mut evidence = anodizer_core::PublishEvidence::new("homebrew");
        // Only record rollback targets when at least one push was made.
        // The rollback path's existing empty-check then short-circuits
        // correctly when nothing was published.
        if any_pushed {
            let targets = collect_run_targets(ctx);
            evidence.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
                homebrew_targets: targets,
            });
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        // Dedup by `(repo_url, branch)` so a tap that holds multiple
        // formulae/casks isn't reverted twice (second revert undoes
        // the first).
        let targets = decode_homebrew_targets(&evidence.extra);
        let unique = dedup_homebrew_targets(&targets);
        crate::util::run_token_revert_rollback(
            ctx,
            &unique,
            "homebrew",
            "HOMEBREW_TAP_TOKEN",
            "tap clone targets",
            "tap",
        )
    }

    /// Probe every active tap repo (formula + cask) for existence + push scope
    /// before any publisher runs: a missing tap or a token without push access
    /// fails the `git push` after sibling publishers may already have shipped.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        // Formula entries are crate-keyed; cask entries are a top-level list —
        // two universes, both probed against the same tap token.
        let formulae = crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "HOMEBREW_TAP_TOKEN",
            ctx.config
                .crate_universe()
                .into_iter()
                .filter_map(|c| c.publish.as_ref().and_then(|p| p.homebrew.as_ref())),
            |h| {
                // Homebrew formula has no `skip` field; gate on skip_upload + if.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    h.skip_upload.as_ref(),
                    h.if_condition.as_deref(),
                )
            },
            |h| h.repository.as_ref(),
        );
        let casks = crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "HOMEBREW_TAP_TOKEN",
            ctx.config.homebrew_casks.iter().flatten(),
            |cask| {
                // Casks have no `skip` field; gate on skip_upload + if only.
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    None,
                    cask.skip_upload.as_ref(),
                    cask.if_condition.as_deref(),
                )
            },
            |cask| cask.repository.as_ref(),
        );
        Ok(formulae.merge(casks))
    }
}

/// The crate-level `publish.homebrew` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::HomebrewConfig> {
    p.homebrew.as_ref()
}

pub(crate) fn is_homebrew_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{
        CrateConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig, RepositoryConfig,
    };
    use anodizer_core::log::{LogCapture, LogLevel};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn homebrew_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        branch: Some("main".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn homebrew_publisher_classification() {
        let p = HomebrewPublisher::new();
        assert_eq!(p.name(), "homebrew");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn homebrew_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = HomebrewPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn homebrew_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("homebrew");
        let p = HomebrewPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("homebrew")
                && m.contains("tap clone targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn homebrew_target_extra_carries_no_secret_material() {
        // Structural pin: build an evidence with a populated variant,
        // serialize, assert (a) no credential-shaped keys appear AND
        // (b) the operator-public shape is preserved. The type system
        // pins the negative half (the snapshot struct has no token
        // field to land in); this test pins the positive half.
        let mut e = PublishEvidence::new("homebrew");
        e.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
            homebrew_targets: vec![HomebrewTarget {
                target: "demo".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            }],
        });
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: the env-var NAME is operator-public and
        // must serialize; ditto target/repo_url/branch.
        assert!(s.contains("HOMEBREW_TAP_TOKEN"), "{s}");
        assert!(s.contains("\"target\":\"demo\""), "{s}");
        assert!(
            s.contains("\"repo_url\":\"https://github.com/acme/homebrew-tap.git\""),
            "{s}"
        );
        assert!(s.contains("\"branch\":\"main\""), "{s}");
    }

    #[test]
    fn homebrew_target_extra_roundtrips() {
        // Build a typed-extra evidence shaped like what `run` would emit
        // and check the decode path returns the same Vec.
        let original = vec![
            HomebrewTarget {
                target: "demo".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
            HomebrewTarget {
                target: "demo-cask".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: None,
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
        ];
        let extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
            homebrew_targets: original.clone(),
        });
        let decoded = decode_homebrew_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn homebrew_effective_publish_crates_implicit_all_when_selection_empty() {
        // Regression pin for the `selected_crates = Vec::new()` failure
        // mode: the run path used to iterate the empty Vec and silently
        // skip every configured tap. The helper now resolves to
        // implicit-all over `publish.homebrew`-carrying crates.
        let ctx = TestContextBuilder::new()
            .crates(vec![
                homebrew_crate("alpha"),
                homebrew_crate("beta"),
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        let names = crate::publisher_helpers::effective_publish_crates(
            &ctx,
            is_homebrew_per_crate_configured,
        );
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn homebrew_effective_publish_crates_honors_non_empty_selection() {
        let ctx = TestContextBuilder::new()
            .crates(vec![homebrew_crate("alpha"), homebrew_crate("beta")])
            .selected_crates(vec!["beta".to_string()])
            .build();
        let names = crate::publisher_helpers::effective_publish_crates(
            &ctx,
            is_homebrew_per_crate_configured,
        );
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn homebrew_collect_run_targets_includes_per_crate_and_top_level() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![homebrew_crate("demo")])
            .build();
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("demo-cask".into()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".into()),
                name: Some("homebrew-cask".into()),
                branch: Some("main".into()),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        let targets = collect_run_targets(&ctx);
        assert_eq!(targets.len(), 2, "expected 1 per-crate + 1 top-level cask");
        let names: Vec<&str> = targets.iter().map(|t| t.target.as_str()).collect();
        assert!(names.contains(&"demo"), "{names:?}");
        assert!(names.contains(&"demo-cask"), "{names:?}");
    }

    #[test]
    fn homebrew_rollback_dedups_shared_tap() {
        // 3 targets pointing at the same tap collapse to 1. The
        // shape mirrors how a workspace with 3 crates plus a
        // same-tap cask would be recorded. Test the dedup helper
        // directly — invoking rollback would require a real git
        // remote (covered by `git_revert_and_push_*` tests).
        let targets = vec![
            HomebrewTarget {
                target: "alpha".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
            HomebrewTarget {
                target: "beta".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
            HomebrewTarget {
                target: "gamma".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
        ];
        let unique = dedup_homebrew_targets(&targets);
        assert_eq!(
            unique.len(),
            1,
            "expected one revert per tap, got {unique:?}"
        );
        assert_eq!(unique[0].target, "alpha");

        // Different branches on the same repo stay distinct — they're
        // separate revert targets.
        let cross_branch = vec![
            HomebrewTarget {
                target: "alpha".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
            HomebrewTarget {
                target: "beta".into(),
                repo_url: "https://github.com/acme/homebrew-tap.git".into(),
                branch: Some("legacy".into()),
                token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
            },
        ];
        let unique = dedup_homebrew_targets(&cross_branch);
        assert_eq!(unique.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary.

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("starting homebrew publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(
            msg.starts_with("skipped homebrew for crate 'demo'"),
            "{msg}"
        );
        assert!(msg.contains("no homebrew config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(
            msg.starts_with("starting per-crate homebrew publish"),
            "{msg}"
        );
        assert!(msg.contains("'demo'"), "{msg}");
    }

    /// Build a two-crate workspace context where `unconfigured` carries no
    /// `publish.homebrew` block, attach a [`LogCapture`], and exercise the
    /// per-crate skip site's exact call shape
    /// (`log.skip_line(ctx.options.show_skipped, …)`) for the unconfigured
    /// crate. Returns the recorded `(level, message)` lines.
    fn capture_homebrew_skip(show_skipped: bool, verbose: bool) -> Vec<(LogLevel, String)> {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                homebrew_crate("configured"),
                homebrew_crate("unconfigured"),
            ])
            .show_skipped(show_skipped)
            .verbose(verbose)
            .build();
        // Strip the homebrew block from the second crate so the per-crate
        // configured predicate fails for it — the exact no-op skip branch.
        ctx.config.crates[1].publish = None;
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());
        let log = ctx.logger("homebrew");
        let crate_name = "unconfigured";
        assert!(
            !is_homebrew_per_crate_configured(&ctx, crate_name),
            "fixture must leave the crate unconfigured so the skip branch fires"
        );
        log.skip_line(
            ctx.options.show_skipped,
            &run_skip_unconfigured_message(crate_name),
        );
        cap.all_messages()
    }

    #[test]
    fn homebrew_no_config_skip_is_debug_level_by_default() {
        // Default (show_skipped=false, Normal verbosity): the per-crate
        // "no homebrew config block" line is recorded at Debug, NOT Status,
        // so workspace mode does not emit one such line per non-applicable
        // crate at default verbosity.
        let lines = capture_homebrew_skip(false, false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, LogLevel::Debug, "{lines:?}");
        assert!(lines[0].1.contains("no homebrew config block"), "{lines:?}");
        assert!(
            lines.iter().all(|(l, _)| *l != LogLevel::Status),
            "no-config skip must not record at Status by default: {lines:?}"
        );
    }

    #[test]
    fn homebrew_no_config_skip_surfaces_with_show_skipped() {
        // --show-skipped forces the line back to Status for diagnosis.
        let lines = capture_homebrew_skip(true, false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, LogLevel::Status, "{lines:?}");
    }

    #[test]
    fn homebrew_no_config_skip_surfaces_at_debug_verbosity() {
        // Without --show-skipped (show_skipped=false) but at --debug
        // verbosity, the no-op skip still surfaces: skip_line routes to
        // debug(), which prints at Verbosity::Debug. Mirrors the default
        // fixture exactly, differing only in the logger's verbosity.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                homebrew_crate("configured"),
                homebrew_crate("unconfigured"),
            ])
            .show_skipped(false)
            .debug(true)
            .build();
        ctx.config.crates[1].publish = None;
        let cap = LogCapture::new();
        ctx.with_log_capture(cap.clone());
        let log = ctx.logger("homebrew");
        let crate_name = "unconfigured";
        assert!(
            !is_homebrew_per_crate_configured(&ctx, crate_name),
            "fixture must leave the crate unconfigured so the skip branch fires"
        );
        log.skip_line(
            ctx.options.show_skipped,
            &run_skip_unconfigured_message(crate_name),
        );
        let lines = cap.all_messages();
        assert_eq!(cap.debug_count(), 1, "{lines:?}");
        let debug_lines: Vec<&String> = lines
            .iter()
            .filter(|(l, _)| *l == LogLevel::Debug)
            .map(|(_, m)| m)
            .collect();
        assert_eq!(debug_lines.len(), 1, "{lines:?}");
        assert!(
            debug_lines[0].contains("no homebrew config block"),
            "{lines:?}"
        );
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2, 0);
        assert!(msg.starts_with("finished homebrew publish"), "{msg}");
        // Two formula crates, no casks → total unit count is 2.
        assert!(msg.contains("2 configured unit(s) processed"), "{msg}");
        assert!(msg.contains("2 formula crate(s)"), "{msg}");
        assert!(msg.contains("0 cask(s)"), "{msg}");
    }

    /// A cask-only run (zero formula crates, one published cask) must report
    /// a NON-zero processed count — the v0.9.1 log read "0 crate(s)
    /// processed" even though the cask published, which looked like a no-op.
    #[test]
    fn run_done_message_counts_published_casks() {
        let msg = run_done_message(0, 1);
        assert!(
            msg.contains("1 configured unit(s) processed"),
            "cask-only run must report 1 unit, not 0; got: {msg}"
        );
        assert!(msg.contains("1 cask(s)"), "{msg}");
        // Mixed: 1 formula + 2 casks → 3 units total.
        let mixed = run_done_message(1, 2);
        assert!(mixed.contains("3 configured unit(s) processed"), "{mixed}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("homebrew publisher registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
        // Both sub-surfaces must be named so an operator on the cask-only
        // path knows that configuring a cask also satisfies the publisher.
        assert!(msg.contains("publish.homebrew"), "{msg}");
        assert!(msg.contains("homebrew_casks"), "{msg}");
    }

    /// The no-eligible-crates warning must fire only when the iteration
    /// loop's configured-predicate filtered every selected crate out — NOT
    /// when `publish_to_homebrew` returned `Ok(false)` because of dry-run /
    /// skip_upload short-circuits. Incrementing `processed` only on
    /// push-success would make this predicate return `true` in dry-run with
    /// a configured crate, emitting a spurious warning for an
    /// otherwise-correct run.
    #[test]
    fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
        // Dry-run with one configured crate: `processed` increments on
        // crate-entry (1), so the warning must not fire.
        assert!(!should_warn_no_eligible(1, 1, 0));
        // True positive: 3 crates selected, none configured for homebrew,
        // and no top-level casks → warning fires.
        assert!(should_warn_no_eligible(0, 3, 0));
        // Boundary: empty selection (no crates configured at all) → no
        // warning. The warn would be noise when there's nothing the
        // operator could change about --crate/--all to fix it.
        assert!(!should_warn_no_eligible(0, 0, 0));
        // Partial-skip: 2 of 3 selected crates were unconfigured, 1 ran
        // → no warning.
        assert!(!should_warn_no_eligible(1, 3, 0));
    }

    /// Cask-only project (the recommended path): zero `publish.homebrew`
    /// formula blocks but a top-level `homebrew_casks:` entry. The cask
    /// publishes correctly with `processed == 0`, so the "nothing pushed"
    /// warning must NOT fire — any configured cask suppresses it.
    #[test]
    fn should_warn_no_eligible_suppressed_when_casks_configured() {
        // No formula crates processed, selection non-empty, but 1 cask
        // configured → no warning (the cask is the publish surface).
        assert!(!should_warn_no_eligible(0, 3, 1));
        // Multiple casks, still no formula → still suppressed.
        assert!(!should_warn_no_eligible(0, 1, 4));
        // Both surfaces empty → the warning is a true signal, still fires.
        assert!(should_warn_no_eligible(0, 2, 0));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects a homebrew-configured crate. Verifies the run path is wired
    /// (returns Ok). The false-positive no-eligible-warning regression is
    /// anchored by
    /// `should_warn_no_eligible_only_fires_when_predicate_filtered_everything`
    /// above, which covers the predicate the run path uses.
    #[test]
    fn homebrew_publisher_run_dry_run_returns_ok() {
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![homebrew_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = HomebrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        // dry-run publish_to_homebrew returns false (no actual push),
        // so evidence.extra may be empty — but the run path must not error.
        // The important assertion is that we round-tripped without panic
        // and the publisher returned Ok.
        let _ = decode_homebrew_targets(&evidence.extra);
    }

    /// When the publisher is registered (a crate has a homebrew block) but
    /// the selected-crates filter excludes every homebrew-configured crate,
    /// the run path must still return Ok (so the dispatch chain doesn't
    /// abort), but record no targets — and the operator-facing warning
    /// helper must produce a remediation-pointing string.
    #[test]
    fn homebrew_publisher_run_no_eligible_crates_returns_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                homebrew_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-homebrew crate — publisher registered but
            // run path will iterate zero homebrew-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = HomebrewPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert!(
            evidence.primary_ref.is_none(),
            "no homebrew-eligible crate selected, primary_ref must be unset"
        );
        let targets = decode_homebrew_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "no homebrew-eligible crate selected, targets must be empty"
        );
    }

    /// Cask-only project end-to-end: a top-level `homebrew_casks:` entry and
    /// ZERO `publish.homebrew` formula blocks. The cask path is the
    /// publish surface (`processed == 0`), so the publisher must NOT emit the
    /// false "registered but ... nothing pushed" warning. Regression for the
    /// v0.9.0 log where the cask pushed successfully yet the warning fired.
    #[test]
    fn homebrew_publisher_cask_only_does_not_warn_nothing_pushed() {
        let capture = anodizer_core::log::LogCapture::new();
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            // No `publish.homebrew` formula crate — a plain crate only.
            .crates(vec![CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            }])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        // Cask-only configuration (the recommended path).
        ctx.config.homebrew_casks = Some(vec![HomebrewCaskConfig {
            name: Some("demo".into()),
            repository: Some(RepositoryConfig {
                owner: Some("acme".into()),
                name: Some("homebrew-tap".into()),
                branch: Some("main".into()),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        ctx.with_log_capture(capture.clone());

        let p = HomebrewPublisher::new();
        p.run(&mut ctx).expect("cask-only publisher.run ok");

        let warns = capture.warn_messages();
        assert!(
            !warns.iter().any(|m| m.contains("nothing pushed")),
            "cask-only config must not emit the false nothing-pushed warning; got: {warns:?}"
        );
    }

    /// The genuinely-empty case (neither formula nor cask configured) must
    /// still emit the warning so a misconfigured publisher is not silent.
    #[test]
    fn homebrew_publisher_truly_empty_still_warns_nothing_pushed() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                homebrew_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-homebrew crate, and configure no casks.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        ctx.with_log_capture(capture.clone());

        let p = HomebrewPublisher::new();
        p.run(&mut ctx).expect("publisher.run ok");

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("nothing pushed")),
            "truly-empty homebrew config must warn; got: {warns:?}"
        );
    }

    #[test]
    fn homebrew_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![homebrew_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = HomebrewPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }
}
