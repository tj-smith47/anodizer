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
        // Snapshot the rollback targets from config BEFORE the publish
        // path runs. Config is read-only during publish so this could
        // run after too, but capturing here keeps the `run` body
        // symmetric for every Bundle B publisher.
        let targets = collect_run_targets(ctx);

        // Per-crate formulae (delegates to the existing entrypoint,
        // body unchanged per the Bundle B contract).
        let selected = ctx.options.selected_crates.clone();
        for crate_name in &selected {
            if !is_homebrew_per_crate_configured(ctx, crate_name) {
                continue;
            }
            super::publish_to_homebrew(ctx, crate_name, &log)?;
        }
        // Top-level casks (single invocation; the entrypoint itself
        // iterates over `ctx.config.homebrew_casks`).
        super::publish_top_level_homebrew_casks(ctx, &log)?;

        let mut evidence = anodizer_core::PublishEvidence::new("homebrew");
        evidence.extra = serde_json::json!({ "homebrew_targets": targets });
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
}
