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
        // Only record rollback targets for overlay repos this run
        // actually mutated. See `HomebrewPublisher::run` for the
        // long-form rationale: intent-driven evidence makes the
        // rollback orchestrator git-revert HEAD in clones it never
        // touched, which fails on missing identity AND would
        // otherwise revert the wrong commit.
        let mut any_pushed = false;
        for crate_name in &selected {
            if !is_nix_per_crate_configured(ctx, crate_name) {
                continue;
            }
            if super::publish_to_nix(ctx, crate_name, &log)? {
                any_pushed = true;
            }
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
}
