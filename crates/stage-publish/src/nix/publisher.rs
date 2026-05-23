//! `NixPublisher` — Bundle B Manager-group `Publisher` impl that wraps
//! the existing [`publish_to_nix`](super::publish_to_nix) per-crate
//! entry point.
//!
//! Rollback shape mirrors the other Bundle B publishers: every push
//! to the configured nix overlay repo is recorded so a `--rollback-
//! only` re-clones, runs `git revert HEAD --no-edit`, and pushes the
//! revert back to the same branch.
//!
//! CREDENTIAL HANDLING: [`NixTarget`] stores `token_env_var` — the
//! NAME of the env var — not the resolved token VALUE. The token is
//! read from the live env at rollback time so persisted evidence
//! carries no secret material. Same rule applies to the homebrew /
//! scoop Bundle B publishers.

use anodizer_core::context::Context;
use serde::{Deserialize, Serialize};

use crate::util::{RevertTarget, run_revert_targets_parallel};

simple_publisher!(
    NixPublisher,
    "nix",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NixTarget {
    target: String,
    repo_url: String,
    branch: Option<String>,
    token_env_var: Option<String>,
}

fn decode_nix_targets(extra: &serde_json::Value) -> Vec<NixTarget> {
    extra
        .get("nix_targets")
        .and_then(|v| serde_json::from_value::<Vec<NixTarget>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Collapse recorded overlay-push targets to a unique set keyed by
/// `(repo_url, branch)`. First entry seen wins. See homebrew's
/// `dedup_homebrew_targets` for the same-revert-twice hazard.
fn dedup_nix_targets(targets: &[NixTarget]) -> Vec<NixTarget> {
    let mut seen: std::collections::BTreeSet<(String, Option<String>)> =
        std::collections::BTreeSet::new();
    let mut out: Vec<NixTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        let key = (t.repo_url.clone(), t.branch.clone());
        if seen.insert(key) {
            out.push(t.clone());
        }
    }
    out
}

fn collect_nix_run_targets(ctx: &Context) -> Vec<NixTarget> {
    let mut out: Vec<NixTarget> = Vec::new();
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(nc) = c.publish.as_ref().and_then(|p| p.nix.as_ref()) else {
            continue;
        };
        if let Some((owner, name)) = crate::util::resolve_repo_owner_name(nc.repository.as_ref()) {
            out.push(NixTarget {
                target: c.name.clone(),
                repo_url: format!("https://github.com/{}/{}.git", owner, name),
                branch: crate::util::resolve_branch(nc.repository.as_ref()).map(str::to_string),
                token_env_var: Some("NIX_PKGS_TOKEN".to_string()),
            });
        }
    }
    out
}

fn is_nix_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.nix.is_some()))
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "nix: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.nix` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!("nix: skipping crate '{}' — no nix config block", crate_name)
}

/// Message emitted just before delegating to `publish_to_nix`. Anchors
/// the nix activity (overlay derivation render, repo clone, push) to a
/// specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("nix: starting per-crate publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_nix` on (not the
/// count of successful overlay pushes — `publish_to_nix` has its own
/// skip paths for skip_upload/dry-run/etc., each of which logs its own
/// status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("nix: completed — {} crate(s) processed", processed)
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_nix_per_crate_configured`
/// check passed. `selected_len` is the size of the implicit-all-resolved
/// selection. The dry-run / skip_upload paths inside `publish_to_nix`
/// return Ok(false) without pushing — `processed` must still increment
/// for them, otherwise this predicate fires a false-positive warning even
/// though the correct code path ran.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.nix` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.nix`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every nix-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "nix: registered but 0 of {} effective crate(s) had a nix \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.nix block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for NixPublisher {
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
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_nix_per_crate_configured);
        log.status(&run_start_message(selected.len()));
        // `processed` counts crates whose configured predicate passed and
        // whose `publish_to_nix` invocation was reached — NOT crates that
        // pushed. The dry-run / skip_upload paths inside `publish_to_nix`
        // return Ok(false) without pushing; that's still a successful run
        // of the correct code path, so it must not trigger the
        // no-eligible-crates warning. `any_pushed` (below) tracks the
        // orthogonal "did we mutate an overlay" question used to gate
        // evidence recording.
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_nix_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            processed += 1;
            log.status(&run_per_crate_start_message(crate_name));
            if super::publish_to_nix(ctx, crate_name, &log)? {
                any_pushed = true;
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("nix");
        if any_pushed {
            let targets = collect_nix_run_targets(ctx);
            evidence.extra = serde_json::json!({ "nix_targets": targets });
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_nix_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "nix",
                "overlay clone targets",
            ));
            return Ok(());
        }
        let unique = dedup_nix_targets(&targets);
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
        let env_hint = unique
            .first()
            .and_then(|t| t.token_env_var.as_deref())
            .unwrap_or("NIX_PKGS_TOKEN");
        let (reverted, failed) =
            run_revert_targets_parallel(&prepared, "nix", Some(env_hint), &log);
        log.status(&format!(
            "nix: reverted {} overlay(s), {} failure(s)",
            reverted, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, NixConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn nix_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        branch: Some("master".to_string()),
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
    fn nix_publisher_classification() {
        let p = NixPublisher::new();
        assert_eq!(p.name(), "nix");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn nix_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = NixPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn nix_rollback_warns_when_no_targets_recorded() {
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("nix");
        let p = NixPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let msg =
            crate::publisher_helpers::rollback_empty_warning_msg("nix", "overlay clone targets");
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("overlay clone targets"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    #[test]
    fn nix_target_extra_carries_no_secret_material() {
        // Defense-in-depth: serialize a target and assert no field
        // names that could leak a token / pat / password are present.
        // Mirrors the Bundle B credential-handling contract documented
        // on `PublishEvidence::extra`.
        let t = NixTarget {
            target: "demo".into(),
            repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
            branch: Some("master".into()),
            token_env_var: Some("NIX_PKGS_TOKEN".into()),
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        // The env-var NAME is fine; values must never appear.
        assert!(s.contains("NIX_PKGS_TOKEN"), "{s}");
    }

    #[test]
    fn nix_target_extra_roundtrips() {
        let original = vec![NixTarget {
            target: "demo".into(),
            repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
            branch: Some("master".into()),
            token_env_var: Some("NIX_PKGS_TOKEN".into()),
        }];
        let extra = serde_json::json!({ "nix_targets": original.clone() });
        let decoded = decode_nix_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn nix_effective_publish_crates_implicit_all_when_selection_empty() {
        // Regression pin for the `selected_crates = Vec::new()` failure
        // mode: the run path used to iterate the empty Vec and silently
        // skip every configured overlay. The helper now resolves to
        // implicit-all over `publish.nix`-carrying crates.
        let ctx = TestContextBuilder::new()
            .crates(vec![
                nix_crate("alpha"),
                nix_crate("beta"),
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_nix_per_crate_configured);
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn nix_effective_publish_crates_honors_non_empty_selection() {
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("alpha"), nix_crate("beta")])
            .selected_crates(vec!["beta".to_string()])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_nix_per_crate_configured);
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn nix_collect_run_targets_walks_per_crate_config() {
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("demo")])
            .build();
        let targets = collect_nix_run_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "demo");
        assert_eq!(targets[0].branch.as_deref(), Some("master"));
    }

    #[test]
    fn nix_rollback_dedups_shared_overlay_repo() {
        // A single overlay repo can hold multiple flakes; dedup so
        // the second `git revert HEAD` doesn't undo the first.
        let targets = vec![
            NixTarget {
                target: "alpha".into(),
                repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
                branch: Some("master".into()),
                token_env_var: Some("NIX_PKGS_TOKEN".into()),
            },
            NixTarget {
                target: "beta".into(),
                repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
                branch: Some("master".into()),
                token_env_var: Some("NIX_PKGS_TOKEN".into()),
            },
        ];
        let unique = dedup_nix_targets(&targets);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].target, "alpha");
    }

    // -----------------------------------------------------------------------
    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary.

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("skipping crate 'demo'"), "{msg}");
        assert!(msg.contains("no nix config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("nix:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// The no-eligible-crates warning must fire only when the iteration
    /// loop's configured-predicate filtered every selected crate out — NOT
    /// when `publish_to_nix` returned `Ok(false)` because of dry-run /
    /// skip_upload short-circuits.
    #[test]
    fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
        // Dry-run with one configured crate: `processed` increments on
        // crate-entry (1), so warning must not fire.
        assert!(!should_warn_no_eligible(1, 1));
        // True positive: none configured.
        assert!(should_warn_no_eligible(0, 3));
        // Empty selection → no warning.
        assert!(!should_warn_no_eligible(0, 0));
        // Partial-skip → no warning.
        assert!(!should_warn_no_eligible(1, 3));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects a nix-configured crate. Verifies the run path is wired
    /// (returns Ok). The bug-1 regression is anchored by
    /// `should_warn_no_eligible_only_fires_when_predicate_filtered_everything`.
    #[test]
    fn nix_publisher_run_dry_run_returns_ok() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .build();
        let p = NixPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        // dry-run publish_to_nix returns false (no actual push), so
        // evidence.extra will be empty — the run path must not error.
        let _ = decode_nix_targets(&evidence.extra);
    }

    /// When the publisher is registered (a crate has a nix block) but the
    /// selected-crates filter excludes every nix-configured crate, the run
    /// path must still return Ok and record no targets.
    #[test]
    fn nix_publisher_run_no_eligible_crates_returns_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                nix_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-nix crate — publisher registered but
            // run path will iterate zero nix-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = NixPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert!(
            evidence.primary_ref.is_none(),
            "no nix-eligible crate selected, primary_ref must be unset"
        );
        let targets = decode_nix_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "no nix-eligible crate selected, targets must be empty"
        );
    }
}
