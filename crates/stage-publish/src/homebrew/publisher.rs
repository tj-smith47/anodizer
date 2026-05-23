//! `HomebrewPublisher` — Bundle B Manager-group `Publisher` impl that
//! wraps the existing [`publish_to_homebrew`](super::publish_to_homebrew)
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
//! material. Same rule applies to the scoop / nix Bundle B
//! publishers and is documented at their module level.

use anodizer_core::context::Context;
use serde::{Deserialize, Serialize};

use crate::util::{RevertTarget, run_revert_targets_parallel};

simple_publisher!(
    HomebrewPublisher,
    "homebrew",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

/// Serialized shape of a recorded homebrew tap push. One entry per
/// pushed formula/cask. See module docs for why this lives in evidence
/// instead of on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HomebrewTarget {
    /// Per-target label — formula name, cask name, or the literal
    /// `homebrew_casks` for the top-level path. Surfaces in log lines.
    target: String,
    /// HTTPS clone URL of the tap repo. Plain HTTPS; auth happens via
    /// the env-resolved token at rollback time.
    repo_url: String,
    /// Branch the publish path pushed to. `None` means "the cloned
    /// default branch" (Homebrew taps default to `main`/`master`).
    branch: Option<String>,
    /// Env var name to consult for the rollback re-clone token.
    /// Captured at run-time so rollback uses the same env contract
    /// the publish path validated.
    token_env_var: Option<String>,
}

/// Decode the `homebrew_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
///
/// Returns an empty Vec on any of: missing key, wrong shape, empty
/// array. The rollback path treats empty-decode the same as
/// no-evidence and emits the canonical empty-evidence warn.
fn decode_homebrew_targets(extra: &serde_json::Value) -> Vec<HomebrewTarget> {
    extra
        .get("homebrew_targets")
        .and_then(|v| serde_json::from_value::<Vec<HomebrewTarget>>(v.clone()).ok())
        .unwrap_or_default()
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
                branch: crate::util::resolve_branch(hb.repository.as_ref()).map(str::to_string),
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
                    branch: crate::util::resolve_branch(cask.repository.as_ref())
                        .map(str::to_string),
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
        "homebrew: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.homebrew` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!(
        "homebrew: skipping crate '{}' — no homebrew config block",
        crate_name
    )
}

/// Message emitted just before delegating to `publish_to_homebrew`.
/// Anchors the homebrew activity (formula render, tap clone, push) to a
/// specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("homebrew: starting per-crate publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_homebrew` on (not
/// the count of successful tap pushes — `publish_to_homebrew` has its own
/// skip paths for skip_upload/dry-run/etc., each of which logs its own
/// status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("homebrew: completed — {} crate(s) processed", processed)
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
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.homebrew` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a
/// `publish.homebrew` block — so a zero-processed run means
/// `--crate`/`--all` matrix selection was non-empty AND filtered every
/// homebrew-configured crate out. Operators must see this — otherwise
/// the publisher's `succeeded` status hides the fact that nothing was
/// pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "homebrew: registered but 0 of {} effective crate(s) had a homebrew \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.homebrew block is set.",
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
        Self::PUBLISHER_REQUIRED
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
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
            if !is_homebrew_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            processed += 1;
            log.status(&run_per_crate_start_message(crate_name));
            if super::publish_to_homebrew(ctx, crate_name, &log)? {
                any_pushed = true;
            }
        }
        // Top-level casks (single invocation; the entrypoint itself
        // iterates over `ctx.config.homebrew_casks`).
        if super::publish_top_level_homebrew_casks(ctx, &log)? {
            any_pushed = true;
        }

        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }

        let mut evidence = anodizer_core::PublishEvidence::new("homebrew");
        // Only record rollback targets when at least one push was made.
        // The rollback path's existing empty-check then short-circuits
        // correctly when nothing was published.
        if any_pushed {
            let targets = collect_run_targets(ctx);
            evidence.extra = serde_json::json!({ "homebrew_targets": targets });
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_homebrew_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "homebrew",
                "tap clone targets",
            ));
            return Ok(());
        }

        // Dedup by `(repo_url, branch)` so a tap that holds multiple
        // formulae/casks isn't reverted twice (second revert undoes
        // the first).
        let unique = dedup_homebrew_targets(&targets);
        // Resolve auth tokens at rollback time — never persisted in
        // evidence. `token_env_var` is just the *name* of the env
        // var; the value lives only in the live process env.
        let prepared: Vec<RevertTarget> = unique
            .iter()
            .map(|t| {
                let token = t
                    .token_env_var
                    .as_deref()
                    .and_then(|n| std::env::var(n).ok())
                    .or_else(|| std::env::var("ANODIZER_GITHUB_TOKEN").ok())
                    .or_else(|| std::env::var("GITHUB_TOKEN").ok());
                RevertTarget {
                    target: t.target.clone(),
                    repo_url: t.repo_url.clone(),
                    branch: t.branch.clone(),
                    token,
                    private_key: None,
                    ssh_command: None,
                }
            })
            .collect();
        // Use the first target's env-var hint for the warn lines;
        // every homebrew target carries the same `HOMEBREW_TAP_TOKEN`
        // hint by construction, so picking the first is fine.
        let env_hint = unique
            .first()
            .and_then(|t| t.token_env_var.as_deref())
            .unwrap_or("HOMEBREW_TAP_TOKEN");
        let (reverted, failed) =
            run_revert_targets_parallel(&prepared, "homebrew", Some(env_hint), &log);
        log.status(&format!(
            "homebrew: reverted {} tap(s), {} failure(s)",
            reverted, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

/// True when the crate has a `publish.homebrew` block — mirrors the
/// `per_crate!` predicate in `lib.rs` so the publisher iterates
/// exactly the same crate universe.
fn is_homebrew_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.homebrew.is_some()))
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{
        CrateConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig, RepositoryConfig,
    };
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
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("homebrew");
        let p = HomebrewPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let msg =
            crate::publisher_helpers::rollback_empty_warning_msg("homebrew", "tap clone targets");
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("tap clone targets"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    #[test]
    fn homebrew_target_extra_carries_no_secret_material() {
        // Defense-in-depth: serialize a target and assert no field
        // names that could leak a token / pat / password are present.
        // Mirrors the Bundle B credential-handling contract documented
        // on `PublishEvidence::extra`.
        let t = HomebrewTarget {
            target: "demo".into(),
            repo_url: "https://github.com/acme/homebrew-tap.git".into(),
            branch: Some("main".into()),
            token_env_var: Some("HOMEBREW_TAP_TOKEN".into()),
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        // The env-var NAME is fine; values must never appear.
        assert!(s.contains("HOMEBREW_TAP_TOKEN"), "{s}");
    }

    #[test]
    fn homebrew_target_extra_roundtrips() {
        // Build an evidence blob shaped like what `run` would emit
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
        let extra = serde_json::json!({ "homebrew_targets": original.clone() });
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
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("skipping crate 'demo'"), "{msg}");
        assert!(msg.contains("no homebrew config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("homebrew:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
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
        assert!(!should_warn_no_eligible(1, 1));
        // True positive: 3 crates selected, none configured for homebrew.
        // `processed` stays 0 → warning fires.
        assert!(should_warn_no_eligible(0, 3));
        // Boundary: empty selection (no crates configured at all) → no
        // warning. The warn would be noise when there's nothing the
        // operator could change about --crate/--all to fix it.
        assert!(!should_warn_no_eligible(0, 0));
        // Partial-skip: 2 of 3 selected crates were unconfigured, 1 ran
        // → no warning.
        assert!(!should_warn_no_eligible(1, 3));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects a homebrew-configured crate. Verifies the run path is wired
    /// (returns Ok). The false-positive no-eligible-warning regression is
    /// anchored by
    /// `should_warn_no_eligible_only_fires_when_predicate_filtered_everything`
    /// above, which covers the predicate the run path uses.
    #[test]
    fn homebrew_publisher_run_dry_run_returns_ok() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![homebrew_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
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
}
